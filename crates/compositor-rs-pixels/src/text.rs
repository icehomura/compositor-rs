//! The Type tool's pixels and its on-canvas layout: a `LayerTextStyle` becomes glyphs and an
//! `Rgba8Image`, and the same layout answers the editor's line, caret, selection and hit-testing
//! queries.
//!
//! Ported from `Document/TypeTool.swift` (`EditorSession.textImage`, `textBoxSize`,
//! `attributedText`/`textAttributes`, `containsTextRun` and the baseline a click places) and from the
//! layout half of `Rendering/InlineTextEditor.swift`, whose wrapping, caret, selection and
//! hit-testing maths lived in `NSLayoutManager`/`NSTextView`; the AppKit view itself belongs to the
//! UI slice.
//!
//! # Core Text approximations
//!
//! Core Text has no cross-platform equivalent, so the glyph work is fontdue's outline rasterizer
//! with fontdb's font files. Everything the Swift documents is reproduced exactly — the padding, the
//! box size, the line height (`minimumLineHeight` = `maximumLineHeight` = `lineHeight`), the
//! baseline placement, tracking as `.kern`, the word wrapping, the three alignments and the per-run
//! faces and colors. The places where Core Text's own numbers and behavior are approximated, and
//! why:
//!
//! * **Face lookup.** `NSFont(name:size:)` takes a PostScript name or a family name. fontdb queries
//!   families only, so the PostScript name is matched exactly first, then the family, then the
//!   generic sans-serif face (the system font, `NSFont.systemFont(ofSize:)`), then any face at all.
//!   `font_is_installed`/`resolved_font_name` answer the same question without the fallback.
//! * **Font cascade.** Core Text substitutes a system face per character (the cascade list), so
//!   Latin text falls back to a CJK face for `日本語`. Here a character missing from its own face
//!   falls back to the system default face when that one has it, and otherwise keeps the face's
//!   `.notdef` glyph. Walking the whole database per missing character would parse every installed
//!   font on the machine.
//! * **Glyph metrics.** Advances, ascender, descender and line gap come from fontdue (hmtx and hhea,
//!   scaled by `px / unitsPerEm`); Core Text scales and may round or hint them differently. The
//!   numbers are kept fractional, as Core Text's are in a bitmap context.
//! * **Kerning.** fontdue does not expose the `kern` table, so adjacent pairs are not kerned; Core
//!   Text applies the font's kern pairs. Tracking (`style.tracking`) is applied exactly.
//! * **Shaping.** No GSUB/ligature substitution and no bidirectional layout: the characters are
//!   placed one by one left to right, as a Latin text layer sees them.
//! * **Baseline in a line.** With the line height pinned by min/max line height, the baseline sits
//!   `descent` above the line fragment's bottom — the rule `EditorSession.beginText` itself uses
//!   (`padding + lineHeight - descent`), so a click's baseline and the raster's agree. A line set in
//!   several faces uses the line's largest ascent and descent, as Core Text does.
//! * **Line breaking.** `.byWordWrapping` is a greedy break at whitespace runs (the whitespace is
//!   dropped, as Core Text hangs it) and before ideographs, with a word wider than the container
//!   broken by character; Core Text follows UAX #14, whose other break opportunities (hyphens,
//!   punctuation) are not reproduced. A non-breaking space never breaks.
//! * **Alignment.** The line is flushed inside the container by its advance, without Core Text's
//!   flush-factor clamping for a line wider than the container.
//! * **Antialiasing.** fontdue returns a coverage bitmap of the glyph's floor-pixel box with the
//!   sub-pixel offset folded in; the bitmap is drawn at the glyph's exact fractional pen position
//!   through the canvas's mask sampling, which offsets the coverage bilinearly. Core Text
//!   rasterizes the outline analytically at the same position, so a glyph can differ by a fraction
//!   of a coverage step.
//! * **Tabs.** A tab advances to the next 28-point tab stop (Apple's default
//!   `defaultTabInterval`), measured from the container's left edge. The paragraph style carries no
//!   custom stops, and Core Text's own default interval is not otherwise exposed.
//! * **Truncation.** Only the lines the container's height holds are laid out, as Core Text fills a
//!   finite frame; the rest is the overflow the editor marks with a plus.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use compositor_rs_core::buffer::{Gray8Image, Rgba8Image};
use compositor_rs_core::geom::{Point, Rect, Size};
use compositor_rs_core::layer_text::{LayerTextStyle, TextAlignment, TextRange};
use compositor_rs_core::limits::{MAX_SIDE_EXTENT, MAX_SURFACE_EXTENT};
use compositor_rs_core::PaletteColor;
use fontdue::{Font, FontSettings};
use fontdb::{Database, Family, Query};

use crate::canvas::Canvas;

/// The tab stop interval, in layer pixels: `NSParagraphStyle.defaultTabInterval`'s 28 points.
const TAB_INTERVAL: f64 = 28.0;

/// The width the box is measured in, as `textBoxSize` measures in a 100 000-point container — wide
/// enough that only the hard breaks end a line.
const MEASURE_WIDTH: f64 = 100_000.0;

// MARK: - Fonts

/// A face's horizontal metrics at a size, in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FaceMetrics {
    /// The ascender above the baseline.
    pub ascent: f64,
    /// The descender's magnitude below the baseline (`abs(NSFont.descender)`).
    pub descent: f64,
    /// The gap the font asks for between lines.
    pub leading: f64,
}

/// One resolved face at a style's size.
pub struct Face {
    font: Option<Arc<Font>>,
    /// The resolved face's PostScript name — the installed face's, or the fallback's.
    pub name: String,
    /// The face's horizontal metrics at the style's size.
    pub metrics: FaceMetrics,
}

impl Face {
    fn new(font: Option<Arc<Font>>, name: String, size: f64) -> Self {
        let metrics = match &font {
            Some(font) => {
                let metrics = font.horizontal_line_metrics(size as f32);
                FaceMetrics {
                    ascent: metrics.map(|metrics| metrics.ascent as f64).unwrap_or(0.0),
                    descent: metrics.map(|metrics| -(metrics.descent as f64)).unwrap_or(0.0).abs(),
                    leading: metrics.map(|metrics| metrics.line_gap as f64).unwrap_or(0.0),
                }
            }
            // No font is installed at all: Core Text would still have the system font; there is
            // nothing to measure with, so the glyphs take no room and draw nothing.
            None => FaceMetrics::default(),
        };
        Face { font, name, metrics }
    }

    /// The face's font, `None` on a machine with no installed fonts.
    pub fn font(&self) -> Option<&Font> {
        self.font.as_deref()
    }

    /// `NSFont`'s own test: whether the face has a glyph for `character`.
    fn has_glyph(&self, character: char) -> bool {
        self.font().map(|font| font.has_glyph(character)).unwrap_or(false)
    }

    /// The character's advance at `size`, tracking excluded.
    fn advance(&self, character: char, size: f32) -> f64 {
        self.font()
            .map(|font| font.metrics(character, size).advance_width as f64)
            .unwrap_or(0.0)
    }
}

impl std::fmt::Debug for Face {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Face")
            .field("name", &self.name)
            .field("metrics", &self.metrics)
            .finish_non_exhaustive()
    }
}

/// The machine's faces: fontdb's index of them and the parsed `fontdue::Font`s, kept for the life of
/// the process.
struct FontLibrary {
    database: Database,
    /// Requested name → the installed face it names, `None` when nothing matches. A missing name is
    /// looked up once.
    installed: Mutex<HashMap<String, Option<fontdb::ID>>>,
    /// Face → its parsed font, `None` when the face's file could not be parsed.
    fonts: Mutex<HashMap<fontdb::ID, Option<Arc<Font>>>>,
    /// The system font (`NSFont.systemFont(ofSize:)`), resolved once.
    default_face: OnceLock<Option<(fontdb::ID, Arc<Font>)>>,
}

impl FontLibrary {
    fn load() -> Self {
        let mut database = Database::new();
        database.load_system_fonts();
        FontLibrary {
            database,
            installed: Mutex::new(HashMap::new()),
            fonts: Mutex::new(HashMap::new()),
            default_face: OnceLock::new(),
        }
    }

    /// The installed face `NSFont(name:size:)` would find: the PostScript name first (an exact face,
    /// weight and slant included), then the family.
    fn installed_face(&self, name: &str) -> Option<fontdb::ID> {
        if name.is_empty() {
            return None;
        }
        if let Some(known) = lock(&self.installed).get(name) {
            return *known;
        }
        let id = self
            .database
            .faces()
            .find(|face| face.post_script_name.eq_ignore_ascii_case(name))
            .map(|face| face.id)
            .or_else(|| {
                self.database.query(&Query {
                    families: &[Family::Name(name)],
                    ..Default::default()
                })
            });
        lock(&self.installed).insert(name.to_string(), id);
        id
    }

    fn font(&self, id: fontdb::ID) -> Option<Arc<Font>> {
        if let Some(known) = lock(&self.fonts).get(&id) {
            return known.clone();
        }
        let font = self
            .database
            .with_face_data(id, |data, index| {
                Font::from_bytes(data, FontSettings { collection_index: index, ..Default::default() })
                    .ok()
                    .map(Arc::new)
            })
            .flatten();
        lock(&self.fonts).insert(id, font.clone());
        font
    }

    fn default_face(&self) -> Option<(fontdb::ID, Arc<Font>)> {
        self.default_face
            .get_or_init(|| {
                let id = self
                    .database
                    .query(&Query {
                        families: &[Family::SansSerif],
                        ..Default::default()
                    })
                    .or_else(|| self.database.faces().next().map(|face| face.id))?;
                Some((id, self.font(id)?))
            })
            .clone()
    }

    fn post_script_name(&self, id: fontdb::ID) -> String {
        self.database
            .face(id)
            .map(|face| face.post_script_name.clone())
            .unwrap_or_default()
    }
}

/// Locks a cache. The caches behind these locks hold nothing an unwind can break, so poisoning is
/// ignored rather than unwrapped (`parking_lot` is not a dependency of this crate).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

static LIBRARY: LazyLock<FontLibrary> = LazyLock::new(FontLibrary::load);

/// Whether the machine has any font installed. On one that has none the layout still works — glyphs
/// take no room and draw nothing — but Core Text would still have the system font.
pub fn fonts_available() -> bool {
    !LIBRARY.database.is_empty()
}

/// Whether `name` names an installed face: `NSFont(name:size:) != nil`, the check the font menu and
/// the PSD import's missing-face note make. The rasterizer's system-font fallback is not applied.
pub fn font_is_installed(name: &str) -> bool {
    LIBRARY.installed_face(name).is_some()
}

/// The PostScript name of the face `name` names, `None` when it is not installed.
pub fn resolved_font_name(name: &str) -> Option<String> {
    LIBRARY.installed_face(name).map(|id| LIBRARY.post_script_name(id))
}

/// Every installed face's PostScript name, sorted, the way `NSFontManager.shared.availableFonts`
/// lists them (`TypeFontPicker.menuNeedsUpdate`).
pub fn font_names() -> Vec<String> {
    let mut names: Vec<String> = LIBRARY
        .database
        .faces()
        .map(|face| face.post_script_name.clone())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The face `name` is set in, at `size`: the installed face, else the system font
/// (`NSFont(name:size:) ?? NSFont.systemFont(ofSize:)`).
fn resolve_face(name: &str, size: f64) -> Face {
    if let Some(id) = LIBRARY.installed_face(name) {
        if let Some(font) = LIBRARY.font(id) {
            return Face::new(Some(font), LIBRARY.post_script_name(id), size);
        }
    }
    if let Some((id, font)) = LIBRARY.default_face() {
        return Face::new(Some(font), LIBRARY.post_script_name(id), size);
    }
    Face::new(None, name.to_string(), size)
}

/// The resolved face's horizontal metrics at `size` — `NSFont(name:size:)`'s ascender, descender and
/// leading. `descent` is the magnitude, as `EditorSession.beginText` reads it with `abs`.
pub fn face_metrics(name: &str, size: f64) -> FaceMetrics {
    resolve_face(name, size).metrics
}

// MARK: - Layout

/// A character as the layout places it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedGlyph {
    pub character: char,
    /// The character's first UTF-16 offset in the style's `content`.
    pub utf16_offset: isize,
    /// How many UTF-16 units the character takes: 1, or 2 for a surrogate pair.
    pub utf16_length: isize,
    /// The glyph's left edge in layer pixels.
    pub x: f64,
    /// The glyph's advance, tracking excluded. Control characters — a tab — advance without ink.
    pub advance: f64,
    pub color: PaletteColor,
    /// The index of the face in `TextLayout::faces` the glyph is set in.
    pub face: usize,
}

/// One line of laid-out text.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLine {
    /// The line's own UTF-16 span: the glyphs on it, the break after it excluded.
    pub range: TextRange,
    /// The offset the line's span ends at, its break (and any whitespace a soft break dropped)
    /// included: where the next line starts, or the end of the content.
    pub span_end: isize,
    /// The line fragment's rect in layer pixels: the container's width, `line_height` tall.
    pub rect: Rect,
    /// The baseline's y in layer pixels.
    pub baseline: f64,
    /// Where the glyphs start after the alignment.
    pub origin_x: f64,
    /// The glyphs' total advance, tracking included.
    pub advance: f64,
    /// The line's ascent: the largest ascender of the faces on it.
    pub ascent: f64,
    /// The line's descender magnitude: the largest of the faces on it.
    pub descent: f64,
    pub glyphs: Vec<PlacedGlyph>,
}

/// The laid-out text of one style: the box it fills and its lines.
pub struct TextLayout {
    /// The box's size in layer pixels, ceiled — the raster's size (`text_box_size`, ceiled).
    pub size: Size,
    /// The paragraph container's size: the box inset by the padding, at least one pixel.
    pub container: Size,
    /// The gap between the text and its box, in layer pixels (`LayerTextStyle.padding`).
    pub padding: f64,
    /// The line height the style's leading works out to.
    pub line_height: f64,
    pub tracking: f64,
    /// Top to bottom.
    pub lines: Vec<TextLine>,
    /// Whether the content needed more lines than the container's height holds.
    pub truncated: bool,
    faces: Vec<Face>,
    /// The style's font size, the size the glyphs rasterize at.
    font_size: f32,
    /// The number of UTF-16 units in the content.
    units: isize,
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextLayout")
            .field("size", &self.size)
            .field("container", &self.container)
            .field("line_height", &self.line_height)
            .field("lines", &self.lines.len())
            .finish_non_exhaustive()
    }
}

impl TextLayout {
    /// Lays `style` out in its box: the Swift's `textImage` up to `drawGlyphs`, with the container
    /// sized `max(1, width - 2 * padding)` by `max(1, height - 2 * padding)` and only the lines its
    /// height holds.
    pub fn layout(style: &LayerTextStyle) -> TextLayout {
        let faces = Faces::resolve(style);
        let padding = LayerTextStyle::PADDING;
        let size = box_size_with(style, &faces);
        let width = size.width.ceil();
        let height = size.height.ceil();
        let container = Size::new(
            (width - padding * 2.0).max(1.0),
            (height - padding * 2.0).max(1.0),
        );
        let line_height = style.line_height();
        let mut wrapped = wrap(style, &faces, container.width, style.tracking);
        for line in &mut wrapped {
            line.totals(&faces, style.tracking);
        }
        // Core Text fills a finite frame: a line is laid out while its top is inside the container.
        let allowed = line_limit(container.height, line_height);
        let truncated = wrapped.len() > allowed;
        let mut lines: Vec<TextLine> = Vec::with_capacity(wrapped.len().min(allowed));
        for (index, line) in wrapped.into_iter().take(allowed).enumerate() {
            let top = padding + index as f64 * line_height;
            let origin_x = padding
                + match style.alignment {
                    TextAlignment::Left => 0.0,
                    TextAlignment::Center => (container.width - line.advance) / 2.0,
                    TextAlignment::Right => container.width - line.advance,
                };
            let mut x = origin_x;
            let mut glyphs = Vec::with_capacity(line.items.len());
            for item in &line.items {
                glyphs.push(PlacedGlyph {
                    character: item.character,
                    utf16_offset: item.utf16_offset,
                    utf16_length: item.utf16_length,
                    x,
                    advance: item.advance,
                    color: item.color,
                    face: item.face,
                });
                x += item.advance + style.tracking;
            }
            lines.push(TextLine {
                range: line.range,
                span_end: line.span_end,
                rect: Rect::new(padding, top, container.width, line_height),
                baseline: top + line_height - line.descent,
                origin_x,
                advance: line.advance,
                ascent: line.ascent,
                descent: line.descent,
                glyphs,
            });
        }
        TextLayout {
            size: Size::new(width, height),
            container,
            padding,
            line_height,
            tracking: style.tracking,
            lines,
            truncated,
            faces: faces.list,
            font_size: faces.size,
            units: content_units(&style.content),
        }
    }

    /// The faces the layout uses: the style's own face first, then the faces its runs are set in,
    /// then the system font.
    pub fn faces(&self) -> &[Face] {
        &self.faces
    }

    /// The number of UTF-16 units in the content — the offsets the caret, selection and hit-testing
    /// queries are in.
    pub fn content_units(&self) -> isize {
        self.units
    }

    /// Whether the content needed more lines than the container's height holds (the editor draws a
    /// plus in the box's bottom-right handle for it).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Rasterizes the glyphs into `canvas` — the `NSLayoutManager.drawGlyphs` half of `textImage`.
    /// Control characters advance without ink.
    pub fn draw(&self, canvas: &mut Canvas) {
        for line in &self.lines {
            for glyph in &line.glyphs {
                if glyph.character.is_control() {
                    continue;
                }
                let face = &self.faces[glyph.face];
                draw_glyph(
                    canvas,
                    face,
                    glyph.character,
                    self.font_size,
                    glyph.x,
                    line.baseline,
                    glyph.color,
                );
            }
        }
    }

    /// The line fragment rects, top to bottom — `NSLayoutManager.lineFragmentRect(forGlyphAt:)`.
    pub fn line_rects(&self) -> Vec<Rect> {
        self.lines.iter().map(|line| line.rect).collect()
    }

    /// The index of the line that owns `offset`: the last line whose content starts at or before it.
    pub fn line_index(&self, offset: isize) -> usize {
        let mut index = 0;
        for (position, line) in self.lines.iter().enumerate() {
            if line.range.location <= offset {
                index = position;
            } else {
                break;
            }
        }
        index
    }

    /// The insertion point's rect at `offset`: one pixel wide and the line fragment's full height,
    /// as `NSTextView`'s caret is with the style's fixed line height. Offsets outside the content
    /// clamp to its ends, and an offset inside whitespace a soft break dropped sits at the end of
    /// the line it was dropped from.
    pub fn caret_rect(&self, offset: isize) -> Rect {
        let Some(line) = self.lines.get(self.line_index(offset)) else {
            return Rect::new(self.padding, self.padding, 1.0, self.line_height);
        };
        Rect::new(self.caret_x(line, offset), line.rect.min_y(), 1.0, self.line_height)
    }

    /// The rects `NSTextView` fills for `range`, one per line it covers, in line order. An empty or
    /// inverted range selects nothing; a range covering only a line break has no width and yields no
    /// rect.
    pub fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let (start, end) = self.clamp_range(range);
        if end <= start {
            return Vec::new();
        }
        let mut rects = Vec::new();
        for line in &self.lines {
            let line_start = line.range.location;
            if line.span_end <= start || line_start >= end {
                continue;
            }
            let x0 = self.caret_x(line, start.max(line_start));
            let x1 = if end >= line.span_end {
                self.line_end_x(line)
            } else {
                self.caret_x(line, end)
            };
            if x1 > x0 {
                rects.push(Rect::new(x0, line.rect.min_y(), x1 - x0, self.line_height));
            }
        }
        rects
    }

    /// The UTF-16 offset nearest `point` in layer pixels, as `NSTextView.characterIndex(for:)` rounds
    /// a click to the closest character boundary; a tie takes the earlier boundary. A point outside
    /// the text clamps to the nearest line's start or end.
    pub fn offset_for_point(&self, point: Point) -> isize {
        let Some(line) = self.lines.get(self.line_index_for_y(point.y)) else {
            return 0;
        };
        let mut best_offset = line.range.location;
        let mut best_distance = (point.x - line.origin_x).abs();
        for glyph in &line.glyphs {
            let x = glyph.x + glyph.advance + self.tracking;
            let distance = (point.x - x).abs();
            if distance < best_distance {
                best_distance = distance;
                best_offset = glyph.utf16_offset + glyph.utf16_length;
            }
        }
        best_offset
    }

    /// The x a caret at `offset` sits at inside `line`: after every glyph that ends at or before it.
    fn caret_x(&self, line: &TextLine, offset: isize) -> f64 {
        let mut x = line.origin_x;
        for glyph in &line.glyphs {
            if glyph.utf16_offset + glyph.utf16_length <= offset {
                x = glyph.x + glyph.advance + self.tracking;
            } else {
                break;
            }
        }
        x
    }

    /// Where a line's glyphs end: the end of a selection that covers the line's break too.
    fn line_end_x(&self, line: &TextLine) -> f64 {
        line.origin_x + line.advance
    }

    /// The line whose fragment holds `y`, the last one whose top is above it.
    fn line_index_for_y(&self, y: f64) -> usize {
        let mut index = 0;
        for (position, line) in self.lines.iter().enumerate() {
            if y >= line.rect.min_y() {
                index = position;
            } else {
                break;
            }
        }
        index
    }

    /// `range` clamped to the content's UTF-16 offsets, as `NSRange` is read.
    fn clamp_range(&self, range: TextRange) -> (isize, isize) {
        let start = range.location.max(0).min(self.units);
        let end = (range.location + range.length).max(start).min(self.units);
        (start, end)
    }
}

/// The number of lines a container of `container_height` holds: a line is laid out while its top is
/// above the container's bottom.
fn line_limit(container_height: f64, line_height: f64) -> usize {
    if !(line_height > 0.0) {
        return usize::MAX;
    }
    let mut count = 0usize;
    while (count as f64) * line_height < container_height {
        count += 1;
    }
    count
}

/// The rasterizes glyph's coverage into the canvas, in the run's color.
fn draw_glyph(
    canvas: &mut Canvas,
    face: &Face,
    character: char,
    size: f32,
    x: f64,
    baseline: f64,
    color: PaletteColor,
) {
    let Some(font) = face.font() else { return };
    let (metrics, bitmap) = font.rasterize(character, size);
    if metrics.width == 0 || metrics.height == 0 {
        return;
    }
    // The bitmap starts at the glyph's floor-pixel left edge, and its top is
    // `ymin + height` above the baseline (fontdue measures the bottom-most edge from the baseline,
    // y up; the canvas is y down).
    let origin_x = x + metrics.xmin as f64;
    let origin_y = baseline - (metrics.ymin + metrics.height as i32) as f64;
    let mask = Gray8Image::from_data(metrics.width, metrics.height, bitmap);
    canvas.set_fill_color(color);
    canvas.draw_gray(
        &mask,
        Rect::new(origin_x, origin_y, metrics.width as f64, metrics.height as f64),
    );
}

// MARK: - Box size and raster

/// Why a text layer could not be rasterized: `ProjectError.invalid` and `ProjectError.tooLarge`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextError {
    /// The style is not valid (`LayerTextStyle::is_valid`).
    Invalid,
    /// The raster would exceed the side or surface limit.
    TooLarge,
}

impl std::fmt::Display for TextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextError::Invalid => formatter.write_str("the text style is not valid"),
            TextError::TooLarge => formatter.write_str("the text surface would exceed the size limit"),
        }
    }
}

impl std::error::Error for TextError {}

/// How big a text layer is: the fixed box when it has one, else what the text measures plus its
/// padding and a caret's worth of width so an empty line still has somewhere to type
/// (`EditorSession.textBoxSize`).
pub fn text_box_size(style: &LayerTextStyle) -> Size {
    box_size_with(style, &Faces::resolve(style))
}

fn box_size_with(style: &LayerTextStyle, faces: &Faces) -> Size {
    if let Some(box_size) = style.box_size {
        return box_size;
    }
    let measured = wrap(style, faces, MEASURE_WIDTH, style.tracking);
    let width = measured.iter().map(|line| line.advance).fold(0.0, f64::max);
    let height = measured.len() as f64 * style.line_height();
    let line = style.line_height().ceil();
    Size::new(
        (width + LayerTextStyle::PADDING * 2.0 + style.font_size * 0.1).ceil().max(16.0),
        (height.max(line) + LayerTextStyle::PADDING * 2.0).ceil().max(16.0),
    )
}

/// Rasterizes a text layer's letters (`EditorSession.textImage`): a premultiplied sRGB image the size
/// of `text_box_size` ceiled, transparent where there is no ink, with every letter in its own face
/// and color.
pub fn text_image(style: &LayerTextStyle) -> Result<Rgba8Image, TextError> {
    if !style.is_valid() {
        return Err(TextError::Invalid);
    }
    let layout = TextLayout::layout(style);
    let (width, height) = (layout.size.width, layout.size.height);
    if !width.is_finite()
        || !height.is_finite()
        || width < 1.0
        || height < 1.0
        || width > MAX_SIDE_EXTENT
        || height > MAX_SIDE_EXTENT
        || width * height > MAX_SURFACE_EXTENT
    {
        return Err(TextError::TooLarge);
    }
    let mut canvas = Canvas::new_rgba(width as usize, height as usize);
    layout.draw(&mut canvas);
    Ok(canvas.into_rgba())
}

/// The first line's baseline in layer pixels: where a click puts new text's first baseline, the
/// font's descent up from the bottom of the line (`EditorSession.beginText`). The layout's own
/// [`TextLine::baseline`] is the same number when the line is set in the style's own face.
pub fn first_baseline(style: &LayerTextStyle) -> f64 {
    LayerTextStyle::PADDING + style.line_height() - face_metrics(&style.font_name, style.font_size).descent
}

// MARK: - Wrapping

/// A character of a paragraph while it is being wrapped.
struct Item {
    character: char,
    utf16_offset: isize,
    utf16_length: isize,
    /// The glyph's advance, tracking excluded; a tab's is worked out from the pen when it is placed.
    advance: f64,
    face: usize,
    color: PaletteColor,
}

/// One wrapped line, before it is placed in the box.
struct WrappedLine {
    items: Vec<Item>,
    /// The glyphs' own UTF-16 span.
    range: TextRange,
    /// Where the next line starts: the line's break and any whitespace a soft break dropped included.
    span_end: isize,
    /// The glyphs' total advance, tracking included.
    advance: f64,
    ascent: f64,
    descent: f64,
}

impl WrappedLine {
    /// The line's advance and the largest ascent and descent of the faces on it — Core Text takes a
    /// line's ascent and descent from the runs it holds. An empty line is set in the style's own
    /// face, as the paragraph's font is.
    fn totals(&mut self, faces: &Faces, tracking: f64) {
        let mut advance: f64 = 0.0;
        let mut ascent: f64 = 0.0;
        let mut descent: f64 = 0.0;
        for item in &self.items {
            advance += item.advance + tracking;
            ascent = ascent.max(faces.list[item.face].metrics.ascent);
            descent = descent.max(faces.list[item.face].metrics.descent);
        }
        if self.items.is_empty() {
            ascent = faces.list[faces.base].metrics.ascent;
            descent = faces.list[faces.base].metrics.descent;
        }
        self.advance = advance;
        self.ascent = ascent;
        self.descent = descent;
    }
}

/// The faces a style's text is set in, and the runs that name them.
struct Faces {
    list: Vec<Face>,
    /// The style's font runs that `attributedText` applies — `containsTextRun`'s bounds — with the
    /// face each is set in.
    font_runs: Vec<(isize, isize, usize)>,
    /// The style's color runs that `attributedText` applies.
    color_runs: Vec<(isize, isize, PaletteColor)>,
    /// The style's own face, which text outside a run is set in.
    base: usize,
    /// The system font's index in `list`, which a character missing from its own face falls back to.
    default: usize,
    size: f32,
}

impl Faces {
    fn resolve(style: &LayerTextStyle) -> Faces {
        let size = style.font_size as f32;
        let units = content_units(&style.content);
        let mut list: Vec<Face> = Vec::new();
        let mut names: HashMap<String, usize> = HashMap::new();
        // Two names can resolve to the same face — a missing name and the system font — and one
        // entry per resolved face keeps the glyphs' face indices meaningful.
        let index_of = |name: &str, list: &mut Vec<Face>, names: &mut HashMap<String, usize>| -> usize {
            if let Some(index) = names.get(name) {
                return *index;
            }
            let resolved = resolve_face(name, style.font_size);
            let index = list
                .iter()
                .position(|face| face.name == resolved.name)
                .unwrap_or_else(|| {
                    list.push(resolved);
                    list.len() - 1
                });
            names.insert(name.to_string(), index);
            index
        };
        let base = index_of(&style.font_name, &mut list, &mut names);
        let mut font_runs = Vec::new();
        for run in style.font_runs.as_deref().unwrap_or(&[]) {
            if !contains_text_run(run.location, run.length, units) {
                continue;
            }
            let face = index_of(&run.font_name, &mut list, &mut names);
            font_runs.push((run.location, run.length, face));
        }
        let mut color_runs = Vec::new();
        for run in style.color_runs.as_deref().unwrap_or(&[]) {
            if !contains_text_run(run.location, run.length, units) {
                continue;
            }
            color_runs.push((run.location, run.length, PaletteColor::new(run.red, run.green, run.blue)));
        }
        let default = index_of("", &mut list, &mut names);
        Faces {
            list,
            font_runs,
            color_runs,
            base,
            default,
            size,
        }
    }

    /// The face a character is set in: its run's face, else the style's own face.
    fn run_face(&self, runs: &mut usize, offset: isize, base: usize) -> usize {
        while *runs < self.font_runs.len() && self.font_runs[*runs].0 + self.font_runs[*runs].1 <= offset {
            *runs += 1;
        }
        match self.font_runs.get(*runs) {
            Some((location, _, face)) if *location <= offset => *face,
            _ => base,
        }
    }

    /// The color a character is painted in: its run's color, else the style's own.
    fn run_color(&self, runs: &mut usize, offset: isize, base: PaletteColor) -> PaletteColor {
        while *runs < self.color_runs.len() && self.color_runs[*runs].0 + self.color_runs[*runs].1 <= offset {
            *runs += 1;
        }
        match self.color_runs.get(*runs) {
            Some((location, _, color)) if *location <= offset => *color,
            _ => base,
        }
    }

    /// The face a character rasterizes with: its own face when it has the glyph, else the system
    /// font's when that one has it, else its own (the `.notdef` glyph).
    fn glyph_face(&self, face: usize, character: char) -> usize {
        if self.list[face].has_glyph(character) {
            return face;
        }
        if self.list[self.default].has_glyph(character) {
            return self.default;
        }
        face
    }
}

/// Wraps the content into lines, paragraph by paragraph: a hard break ends a line wherever it is,
/// and a paragraph is wrapped greedily inside `container_width` (see the module docs).
fn wrap(style: &LayerTextStyle, faces: &Faces, container_width: f64, tracking: f64) -> Vec<WrappedLine> {
    let base_color = PaletteColor::new(style.red, style.green, style.blue);
    let mut lines: Vec<WrappedLine> = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    let mut font_runs = 0usize;
    let mut color_runs = 0usize;
    let mut offset: isize = 0;
    let mut paragraph_start: isize = 0;
    let mut characters = style.content.chars().peekable();
    while let Some(character) = characters.next() {
        let length = character.len_utf16() as isize;
        if is_line_break(character) {
            offset += length;
            // "\r\n" is one break, as `NSString.lineRange` reads it.
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
                offset += 1;
            }
            flush_paragraph(&mut items, &mut lines, container_width, tracking, paragraph_start, offset);
            paragraph_start = offset;
            continue;
        }
        let face = faces.glyph_face(faces.run_face(&mut font_runs, offset, faces.base), character);
        items.push(Item {
            character,
            utf16_offset: offset,
            utf16_length: length,
            advance: faces.list[face].advance(character, faces.size),
            face,
            color: faces.run_color(&mut color_runs, offset, base_color),
        });
        offset += length;
    }
    flush_paragraph(&mut items, &mut lines, container_width, tracking, paragraph_start, offset);
    for line in &mut lines {
        line.totals(faces, tracking);
    }
    lines
}

/// Wraps one paragraph's items and appends its lines; `start` is the paragraph's first UTF-16 offset
/// and `span_end` where its own span ends, its break included.
fn flush_paragraph(
    items: &mut Vec<Item>,
    lines: &mut Vec<WrappedLine>,
    container_width: f64,
    tracking: f64,
    start: isize,
    span_end: isize,
) {
    let ranges = wrap_paragraph(items, container_width, tracking);
    let last = ranges.len().saturating_sub(1);
    // The ranges index the paragraph's items; the vector shrinks as the lines are taken. Each line
    // first drops the whitespace the break before it left behind, then takes its own items.
    let mut consumed = 0usize;
    for (position, (range_start, range_end)) in ranges.into_iter().enumerate() {
        if range_start > consumed {
            items.drain(..range_start - consumed);
            consumed = range_start;
        }
        let line_items: Vec<Item> = items.drain(..range_end - consumed).collect();
        consumed = range_end;
        let range = match (line_items.first(), line_items.last()) {
            (Some(first), Some(last)) => TextRange::new(
                first.utf16_offset,
                last.utf16_offset + last.utf16_length - first.utf16_offset,
            ),
            // An empty line — an empty paragraph, or one the container's width cannot hold a glyph
            // of — still owns the offset it sits at.
            _ => TextRange::new(start, 0),
        };
        lines.push(WrappedLine {
            items: line_items,
            range,
            span_end: if position == last { span_end } else { 0 },
            advance: 0.0,
            ascent: 0.0,
            descent: 0.0,
        });
    }
    // The lines' spans are filled in once they are all known: each one ends where the next begins.
    let first = lines.len() - (last + 1);
    for index in first..lines.len().saturating_sub(1) {
        lines[index].span_end = lines[index + 1].range.location;
    }
}

/// The greedy word wrap of one paragraph: the item ranges its lines cover. Whitespace a line breaks
/// at is dropped from it, and a word wider than the container breaks by character.
fn wrap_paragraph(items: &mut [Item], container_width: f64, tracking: f64) -> Vec<(usize, usize)> {
    if items.is_empty() {
        return vec![(0, 0)];
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut pen = 0.0f64;
    let mut opportunity: Option<usize> = None;
    let mut index = 0usize;
    while index < items.len() {
        if items[index].character == '\t' {
            // A tab advances to the next stop, counted from the container's left edge.
            let next_stop = (pen / TAB_INTERVAL).floor() * TAB_INTERVAL + TAB_INTERVAL;
            items[index].advance = next_stop - pen;
        }
        let width = items[index].advance + tracking;
        if pen + width > container_width && index > start {
            let break_at = match opportunity {
                Some(break_at) if break_at > start => break_at,
                _ => index,
            };
            ranges.push((start, trim_trailing_whitespace(items, start, break_at)));
            // The break can be at a whitespace opportunity behind the item that overflowed: the
            // items it passed over belong to the new line, so they are measured again.
            start = break_at;
            index = break_at;
            pen = 0.0;
            opportunity = None;
            continue;
        }
        pen += width;
        if is_breakable_space(items[index].character) {
            opportunity = Some(index + 1);
        } else if index > start
            && is_ideographic(items[index].character)
            && !items[index - 1].character.is_whitespace()
        {
            opportunity = Some(index);
        }
        index += 1;
    }
    ranges.push((start, items.len()));
    ranges
}

/// Where a line that breaks at `end` really ends: the whitespace it broke at is dropped.
fn trim_trailing_whitespace(items: &[Item], start: usize, end: usize) -> usize {
    let mut end = end;
    while end > start && is_breakable_space(items[end - 1].character) {
        end -= 1;
    }
    end
}

/// Whether a line may break at `character`: a space, but never a non-breaking one, which Core Text
/// keeps on the line it started on.
fn is_breakable_space(character: char) -> bool {
    character.is_whitespace() && character != '\u{00A0}'
}

/// A paragraph break: the characters `NSString.lineRange` treats as line separators.
fn is_line_break(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// Whether a line may break before or after `character` as it may around an ideograph. Core Text
/// follows UAX #14; these are the ranges that matter for CJK text.
fn is_ideographic(character: char) -> bool {
    matches!(character as u32,
        0x1100..=0x11FF
        | 0x2E80..=0x303F
        | 0x3040..=0x30FF
        | 0x3130..=0x318F
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD)
}

/// The number of UTF-16 units in `content` — the offsets the runs are in.
fn content_units(content: &str) -> isize {
    content.encode_utf16().count() as isize
}

/// `EditorSession.containsTextRun`: `length > 0 && location >= 0 && location <= total - length`.
fn contains_text_run(location: isize, length: isize, total: isize) -> bool {
    length > 0 && location >= 0 && location <= total - length
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::layer_text::{LayerTextColorRun, LayerTextFontRun};

    /// A style whose face is not installed on Windows, so the system-font fallback is exercised
    /// everywhere unless a test says otherwise.
    fn style(content: &str, font_size: f64) -> LayerTextStyle {
        let mut style = LayerTextStyle::default();
        style.content = content.to_string();
        style.font_size = font_size;
        style
    }

    /// A fixed-box style, the paragraph bounds `beginText(in:)` makes.
    fn boxed(content: &str, font_size: f64, width: f64, height: f64) -> LayerTextStyle {
        let mut style = style(content, font_size);
        style.box_size = Some(Size::new(width, height));
        style
    }

    /// Whether the layout has a face to measure with. On a machine with no installed fonts the
    /// layout still works — zero advances, nothing drawn — so the metric assertions are skipped.
    fn has_font(layout: &TextLayout) -> bool {
        layout.faces()[0].font().is_some()
    }

    /// The two numbers agree to a pixel's millionth: the same arithmetic, written in another order.
    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-6
    }

    #[test]
    fn text_box_size_is_the_fixed_box_when_one_is_set() {
        let mut style = style("Text", 72.0);
        style.box_size = Some(Size::new(200.0, 120.0));
        assert_eq!(text_box_size(&style), Size::new(200.0, 120.0));
        assert_eq!(TextLayout::layout(&style).size, Size::new(200.0, 120.0));

        // A fractional box is ceiled to the raster's whole-pixel size, as `textImage` ceils it.
        style.box_size = Some(Size::new(200.5, 120.25));
        assert_eq!(TextLayout::layout(&style).size, Size::new(201.0, 121.0));
        let image = text_image(&style).expect("a valid box rasterizes");
        assert_eq!((image.width(), image.height()), (201, 121));

        // The container is the box inset by the padding, at least one pixel.
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.padding, LayerTextStyle::PADDING);
        assert_eq!(layout.container, Size::new(201.0 - 24.0, 121.0 - 24.0));
        let tiny = boxed("Text", 12.0, 16.0, 16.0);
        assert_eq!(TextLayout::layout(&tiny).container, Size::new(1.0, 1.0));
    }

    #[test]
    fn point_text_box_is_the_measurement_plus_padding_and_a_caret() {
        // An empty content still gets a caret's worth of width and one line's height.
        let empty = style("", 72.0);
        assert_eq!(text_box_size(&empty), Size::new(32.0, 111.0));
        assert_eq!(TextLayout::layout(&empty).size, Size::new(32.0, 111.0));
        assert_eq!(text_image(&empty).expect("an empty text rasterizes").width(), 32);

        // The measurement is the widest line, and a hard break adds a line's height.
        let mut one = style("Text", 72.0);
        let two = style("Text\nText", 72.0);
        let measured = text_box_size(&one).width;
        assert_eq!(text_box_size(&two).width, measured);
        assert!(text_box_size(&two).height > text_box_size(&one).height);

        // Auto leading is 120% of the size, and `leading` replaces it.
        one.leading = 200.0;
        assert!(text_box_size(&one).height >= 200.0 + 24.0);
    }

    #[test]
    fn wrapping_breaks_at_words_inside_the_box() {
        let style = boxed("the quick brown fox jumps over the lazy dog", 24.0, 160.0, 400.0);
        let layout = TextLayout::layout(&style);
        assert!(layout.lines.len() > 1, "the paragraph wraps inside its box");
        if !has_font(&layout) {
            return;
        }
        let limit = layout.container.width;
        for line in &layout.lines {
            assert!(line.advance <= limit + 1e-9, "line advance {} fits {limit}", line.advance);
            assert!(line.glyphs.iter().all(|glyph| glyph.x >= layout.padding - 1e-9));
            // A single glyph too wide for the container is laid out on its own line and overflows,
            // as Core Text overflows it.
            if line.glyphs.len() > 1 {
                assert!(line
                    .glyphs
                    .iter()
                    .all(|glyph| glyph.x + glyph.advance <= layout.padding + limit + 1e-9));
            }
            // The whitespace a line breaks at is dropped, so no line starts with it.
            if let Some(first) = line.glyphs.first() {
                assert!(!first.character.is_whitespace());
            }
        }
        // Every character is on exactly one line, in order.
        let laid: String = layout
            .lines
            .iter()
            .flat_map(|line| line.glyphs.iter().map(|glyph| glyph.character))
            .filter(|character| !character.is_whitespace())
            .collect();
        let content: String = style
            .content
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert_eq!(laid, content);
    }

    #[test]
    fn a_word_wider_than_the_box_breaks_by_character() {
        let style = boxed("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 24.0, 60.0, 400.0);
        let layout = TextLayout::layout(&style);
        assert!(layout.lines.len() > 1, "the long word breaks by character");
        if !has_font(&layout) {
            return;
        }
        for line in &layout.lines {
            assert!(line.glyphs.iter().all(|glyph| glyph.character == 'a'));
            assert!(line.advance <= layout.container.width + 1e-9);
        }
    }

    #[test]
    fn alignment_sets_one_line_inside_the_box() {
        let mut style = boxed("Text", 12.0, 400.0, 200.0);
        let limit = TextLayout::layout(&style).container.width;

        let left = TextLayout::layout(&style);
        assert_eq!(left.lines.len(), 1);
        let advance = left.lines[0].advance;
        assert_eq!(left.lines[0].origin_x, left.padding);
        assert_eq!(left.lines[0].glyphs[0].x, left.padding);
        assert_eq!(left.caret_rect(left.content_units()).min_x(), left.padding + advance);

        style.alignment = TextAlignment::Center;
        let center = TextLayout::layout(&style);
        assert_eq!(center.lines[0].advance, advance, "the alignment does not change the text");
        assert_eq!(center.lines[0].origin_x - center.padding, (limit - advance) / 2.0);

        style.alignment = TextAlignment::Right;
        let right = TextLayout::layout(&style);
        assert_eq!(right.lines[0].origin_x - right.padding, limit - advance);
        assert_eq!(right.caret_rect(right.content_units()).min_x(), right.padding + limit);
        assert_eq!(right.lines[0].glyphs[0].x, right.lines[0].origin_x);
    }

    #[test]
    fn missing_font_falls_back_instead_of_panicking() {
        let mut style = boxed("Missing face", 24.0, 200.0, 120.0);
        style.font_name = "NoSuchFont-1234".to_string();
        style.font_runs = Some(vec![LayerTextFontRun {
            location: 0,
            length: 7,
            font_name: "AlsoMissing-5678".to_string(),
        }]);
        assert!(!font_is_installed("NoSuchFont-1234"));
        assert_eq!(resolved_font_name("NoSuchFont-1234"), None);
        assert!(!font_is_installed(""));

        let image = text_image(&style).expect("a missing face falls back to the system font");
        assert_eq!((image.width(), image.height()), (200, 120));
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines.len(), 1);
        if fonts_available() {
            assert!(has_font(&layout), "the system font stands in");
            assert!(face_metrics("NoSuchFont-1234", 24.0).descent > 0.0);
            let run_face = layout.lines[0].glyphs[0].face;
            assert_eq!(layout.faces()[run_face].name, layout.faces()[0].name);
            assert!(image.pixels().any(|pixel| pixel[3] > 0), "the fallback face drew ink");
        } else {
            assert_eq!(layout.faces()[0].name, "NoSuchFont-1234");
        }
    }

    #[test]
    fn raster_has_transparent_background_and_colored_glyphs() {
        if !fonts_available() {
            return;
        }
        let mut style = style("TYPE", 72.0);
        style.red = 1.0;
        let image = text_image(&style).expect("a valid style rasterizes");
        let mut ink = 0usize;
        let mut clear = 0usize;
        for pixel in image.pixels() {
            if pixel[3] == 0 {
                clear += 1;
                continue;
            }
            ink += 1;
            assert!(
                pixel[0] > 0 && pixel[1] == 0 && pixel[2] == 0,
                "the glyph is premultiplied red: {pixel:?}"
            );
        }
        assert!(ink > 100 && clear > 100, "ink {ink}, clear {clear}");
    }

    #[test]
    fn selected_color_paints_only_those_letters() {
        let mut style = boxed("AAAA BBBB", 72.0, 800.0, 200.0);
        style.color_runs = Some(vec![LayerTextColorRun {
            location: 5,
            length: 4,
            red: 1.0,
            green: 0.0,
            blue: 0.0,
        }]);
        let layout = TextLayout::layout(&style);
        let glyphs = &layout.lines[0].glyphs;
        assert_eq!(glyphs[4].color, PaletteColor::BLACK);
        assert_eq!(glyphs[5].color, PaletteColor::new(1.0, 0.0, 0.0));
        assert_eq!(glyphs[8].color, PaletteColor::new(1.0, 0.0, 0.0));
        if !fonts_available() {
            return;
        }
        let image = text_image(&style).expect("a valid style rasterizes");
        let mut red = 0usize;
        let mut dark = 0usize;
        for pixel in image.pixels() {
            // Opaque pixels only, as `redAndDarkPixels` reads `alphaComponent > 0.9`.
            if pixel[3] < 230 {
                continue;
            }
            if pixel[0] > 204 && pixel[1] < 51 {
                red += 1;
            } else if pixel[0] < 51 {
                dark += 1;
            }
        }
        assert!(red > 50 && dark > 50, "red {red}, dark {dark}");
    }

    #[test]
    fn first_baseline_is_the_click_placement() {
        let style = style("Text", 72.0);
        let descent = face_metrics(&style.font_name, style.font_size).descent;
        assert_eq!(first_baseline(&style), LayerTextStyle::PADDING + style.line_height() - descent);
        assert_eq!(style.line_height(), 72.0 * 1.2);

        // The raster's own first baseline is the same number when the line is set in the style's
        // own face, which is what the Swift's `beginText` assumes.
        let layout = TextLayout::layout(&style);
        assert_eq!(
            layout.lines[0].baseline,
            layout.lines[0].rect.min_y() + layout.line_height - layout.lines[0].descent
        );
        if layout.lines[0].descent == descent {
            assert_eq!(layout.lines[0].baseline, first_baseline(&style));
        }
    }

    #[test]
    fn caret_selection_and_hit_testing_agree() {
        let style = boxed("AB", 40.0, 300.0, 120.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines.len(), 1);
        let line = &layout.lines[0];
        assert_eq!(line.glyphs.len(), 2);
        let (a, b) = (line.glyphs[0].advance, line.glyphs[1].advance);
        let top = layout.padding;

        assert_eq!(
            layout.line_rects(),
            vec![Rect::new(layout.padding, top, layout.container.width, layout.line_height)]
        );
        assert_eq!(layout.caret_rect(0).min_x(), line.origin_x);
        assert!(close(layout.caret_rect(1).min_x(), line.origin_x + a));
        assert!(close(layout.caret_rect(2).min_x(), line.origin_x + a + b));
        assert_eq!(layout.caret_rect(2).height(), layout.line_height);
        assert_eq!(layout.caret_rect(2).min_y(), top);
        // Offsets outside the content clamp to its ends.
        assert_eq!(layout.caret_rect(-5), layout.caret_rect(0));
        assert_eq!(layout.caret_rect(99), layout.caret_rect(2));

        assert!(layout.selection_rects(TextRange::new(2, 0)).is_empty());
        assert!(layout.selection_rects(TextRange::new(5, -3)).is_empty());
        assert_eq!(
            layout.selection_rects(TextRange::new(0, 2)),
            vec![Rect::new(line.origin_x, top, a + b, layout.line_height)]
        );

        // A click rounds to the nearest character boundary.
        assert_eq!(layout.offset_for_point(Point::new(line.origin_x + a * 0.4, top + 4.0)), 0);
        assert_eq!(layout.offset_for_point(Point::new(line.origin_x + a * 0.6, top + 4.0)), 1);
        assert_eq!(
            layout.offset_for_point(Point::new(line.origin_x + a + b * 0.6, top + 4.0)),
            2
        );
        assert_eq!(layout.offset_for_point(Point::new(-100.0, -50.0)), 0);
        assert_eq!(layout.offset_for_point(Point::new(10_000.0, 10_000.0)), 2);
    }

    #[test]
    fn line_rects_advance_by_the_line_height() {
        let mut style = boxed("A\nB\nC", 20.0, 300.0, 400.0);
        style.leading = 100.0;
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.line_height, 100.0);
        assert_eq!(layout.lines.len(), 3);
        for (index, rect) in layout.line_rects().iter().enumerate() {
            assert_eq!(rect.height(), 100.0);
            assert_eq!(rect.min_y(), layout.padding + index as f64 * 100.0);
            assert_eq!(rect.width(), layout.container.width);
        }
        for line in &layout.lines {
            assert_eq!(line.baseline, line.rect.min_y() + 100.0 - line.descent);
        }
    }

    #[test]
    fn tracking_is_added_after_every_glyph() {
        let plain_style = boxed("ABCDE", 40.0, 600.0, 200.0);
        let mut tracked_style = plain_style.clone();
        tracked_style.tracking = 10.0;
        let plain = TextLayout::layout(&plain_style);
        let tracked = TextLayout::layout(&tracked_style);
        assert_eq!(plain.lines.len(), 1);
        assert_eq!(tracked.lines.len(), 1);
        assert_eq!(tracked.lines[0].glyphs.len(), 5);
        // Tracking follows every glyph, the last one included, as `.kern` does.
        assert!(close(tracked.lines[0].advance - plain.lines[0].advance, 50.0));
        assert!(close(tracked.caret_rect(5).min_x() - plain.caret_rect(5).min_x(), 50.0));
        assert_eq!(tracked.lines[0].glyphs[0].x, plain.lines[0].glyphs[0].x);
        for (index, tracked) in tracked.lines[0].glyphs.iter().enumerate() {
            assert!(close(tracked.x - plain.lines[0].glyphs[index].x, 10.0 * index as f64));
        }
    }

    #[test]
    fn tabs_advance_to_the_next_tab_stop() {
        let style = boxed("\tA", 20.0, 400.0, 120.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines[0].glyphs[0].character, '\t');
        assert_eq!(layout.lines[0].glyphs[0].advance, TAB_INTERVAL);
        assert_eq!(layout.lines[0].glyphs[1].x, layout.padding + TAB_INTERVAL);
        assert_eq!(layout.caret_rect(1).min_x(), layout.padding + TAB_INTERVAL);

        // A tab after text reaches the next stop, counted from the container's left edge.
        let mut after = style.clone();
        after.content = "A\tB".to_string();
        let layout = TextLayout::layout(&after);
        let a = layout.lines[0].glyphs[0].advance;
        if a < TAB_INTERVAL {
            assert_eq!(layout.lines[0].glyphs[1].advance, TAB_INTERVAL - a);
            assert_eq!(layout.lines[0].glyphs[2].x, layout.padding + TAB_INTERVAL);
        }
    }

    #[test]
    fn invalid_styles_are_rejected() {
        let mut style = style("Text", f64::NAN);
        assert_eq!(text_image(&style), Err(TextError::Invalid));
        style.font_size = 72.0;
        assert!(text_image(&style).is_ok());
        style.box_size = Some(Size::new(0.0, 100.0));
        assert_eq!(text_image(&style), Err(TextError::Invalid));
        style.box_size = Some(Size::new(100.0, 100.0));
        style.tracking = 1000.1;
        assert_eq!(text_image(&style), Err(TextError::Invalid));
        assert_eq!(TextError::TooLarge.to_string(), "the text surface would exceed the size limit");
        assert_eq!(TextError::Invalid.to_string(), "the text style is not valid");
    }

    #[test]
    fn only_the_lines_the_container_holds_are_laid_out() {
        let mut style = boxed("A\nB\nC", 20.0, 200.0, 200.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines.len(), 3);
        assert!(!layout.truncated());

        // The container is the box inset by the padding: 72 - 24 = 48 holds two 24-pixel lines.
        style.box_size = Some(Size::new(200.0, 72.0));
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines.len(), 2);
        assert!(layout.truncated());

        // A box too short for even one line still lays the first one out.
        style.box_size = Some(Size::new(200.0, 16.0));
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines.len(), 1);
        assert!(layout.truncated());
        assert_eq!(layout.lines[0].rect.min_y(), layout.padding);
    }

    #[test]
    fn font_runs_choose_their_own_face() {
        let mut style = boxed("Hello", 24.0, 300.0, 120.0);
        style.font_runs = Some(vec![LayerTextFontRun {
            location: 0,
            length: 2,
            font_name: "Courier New".to_string(),
        }]);
        let layout = TextLayout::layout(&style);
        let run_face = layout.lines[0].glyphs[0].face;
        let base_face = layout.lines[0].glyphs[2].face;
        if font_is_installed("Courier New") && resolved_font_name("Courier New") != resolved_font_name(&style.font_name) {
            assert_ne!(run_face, base_face, "the run is set in another face");
            assert_eq!(
                layout.faces()[run_face].name,
                resolved_font_name("Courier New").expect("installed")
            );
        }

        // A run outside the content is ignored, as `containsTextRun` ignores it.
        style.font_runs = Some(vec![LayerTextFontRun {
            location: 3,
            length: 9,
            font_name: "Courier New".to_string(),
        }]);
        let layout = TextLayout::layout(&style);
        let first = layout.lines[0].glyphs[0].face;
        assert!(layout.lines[0].glyphs.iter().all(|glyph| glyph.face == first));
        assert_eq!(layout.lines[0].glyphs.len(), 5);
    }

    #[test]
    fn empty_content_still_lays_out_a_line_with_a_caret() {
        let style = boxed("", 40.0, 200.0, 120.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines.len(), 1);
        assert!(layout.lines[0].glyphs.is_empty());
        assert_eq!(layout.lines[0].range, TextRange::new(0, 0));
        assert_eq!(layout.lines[0].span_end, 0);
        assert_eq!(layout.lines[0].advance, 0.0);
        assert_eq!(layout.caret_rect(0).min_x(), layout.padding);
        assert_eq!(layout.offset_for_point(Point::new(150.0, 60.0)), 0);
        assert!(layout.selection_rects(TextRange::new(0, 0)).is_empty());

        // A trailing break leaves a line to type on, with the caret at its start.
        let style = boxed("A\n", 40.0, 200.0, 200.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.lines.len(), 2);
        assert!(layout.lines[1].glyphs.is_empty());
        assert_eq!(layout.lines[1].range, TextRange::new(2, 0));
        assert_eq!(layout.caret_rect(2).min_x(), layout.padding);
        assert_eq!(layout.caret_rect(2).min_y(), layout.padding + layout.line_height);
    }

    #[test]
    fn line_breaks_split_the_content_in_utf16_units() {
        let style = boxed("A\nB\r\nC\rD", 20.0, 300.0, 400.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.content_units(), 8);
        assert_eq!(layout.lines.len(), 4);
        assert_eq!(layout.lines[0].range, TextRange::new(0, 1));
        assert_eq!(layout.lines[0].span_end, 2);
        assert_eq!(layout.lines[1].range, TextRange::new(2, 1));
        // "\r\n" is one break, as `NSString.lineRange` reads it.
        assert_eq!(layout.lines[1].span_end, 5);
        assert_eq!(layout.lines[2].range, TextRange::new(5, 1));
        assert_eq!(layout.lines[2].span_end, 7);
        assert_eq!(layout.lines[3].range, TextRange::new(7, 1));
        assert_eq!(layout.lines[3].span_end, 8);
        // The break itself belongs to the line it ends.
        assert_eq!(layout.line_index(1), 0);
        assert_eq!(layout.line_index(2), 1);
        assert_eq!(layout.line_index(8), 3);

        // A surrogate pair is one glyph of two units.
        let style = boxed("🙂", 24.0, 400.0, 120.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.content_units(), 2);
        assert_eq!(layout.lines[0].glyphs.len(), 1);
        assert_eq!(layout.lines[0].glyphs[0].utf16_length, 2);
        assert_eq!(layout.caret_rect(2).min_x() - layout.caret_rect(0).min_x(), layout.lines[0].advance);
    }

    #[test]
    fn selection_across_a_break_covers_both_lines() {
        let style = boxed("AAAA\nBBBB", 20.0, 400.0, 200.0);
        let layout = TextLayout::layout(&style);
        assert_eq!(layout.content_units(), 9);
        let rects = layout.selection_rects(TextRange::new(2, 5));
        assert_eq!(rects.len(), 2);
        // The first line's share runs to the end of its glyphs, the break included.
        assert_eq!(rects[0].min_y(), layout.padding);
        assert!(close(
            rects[0].max_x(),
            layout.lines[0].origin_x + layout.lines[0].advance
        ));
        assert_eq!(rects[0].height(), layout.line_height);
        // The second line's share starts at its own start.
        assert_eq!(rects[1].min_y(), layout.padding + layout.line_height);
        assert_eq!(rects[1].min_x(), layout.lines[1].origin_x);
        assert!(rects[1].max_x() < layout.lines[1].origin_x + layout.lines[1].advance - 1.0);
        // The covered letters are the ones the range names.
        assert!(rects[0].width() > 0.0 && rects[1].width() > 0.0);

        // A range that covers only the break has no width and fills nothing.
        assert!(layout.selection_rects(TextRange::new(4, 1)).is_empty());

        // A click lands on the line it is over.
        let second = layout.padding + layout.line_height;
        assert_eq!(layout.offset_for_point(Point::new(layout.lines[1].origin_x - 5.0, second + 2.0)), 5);
        assert_eq!(
            layout.offset_for_point(Point::new(10_000.0, second + 2.0)),
            9
        );
        assert_eq!(layout.offset_for_point(Point::new(10_000.0, 0.0)), 4);
    }

    #[test]
    fn a_non_breaking_space_never_breaks_the_line() {
        if !fonts_available() {
            return;
        }
        // A container just wide enough for "aaaa " and no more.
        let probe = TextLayout::layout(&style("aaaa bbbb", 24.0));
        let glyphs = &probe.lines[0].glyphs;
        let words: f64 = glyphs[0..4].iter().map(|glyph| glyph.advance).sum();
        let space = glyphs[4].advance;
        let container = words + space + 0.5;
        let box_width = (container + 24.0).ceil();
        let assert_break = |content: &str, keeps_space: bool| {
            let layout = TextLayout::layout(&boxed(content, 24.0, box_width, 300.0));
            assert_eq!(layout.lines.len(), 2, "{content:?} wraps");
            let first: Vec<char> = layout.lines[0].glyphs.iter().map(|glyph| glyph.character).collect();
            assert_eq!(
                first.contains(&'\u{00A0}'),
                keeps_space,
                "{content:?} first line {first:?}"
            );
        };
        assert_break("aaaa\u{00A0}bbbb", true);
        assert_break("aaaa bbbb", false);
    }

    #[test]
    fn the_same_style_lays_out_the_same_way() {
        let style = boxed("Determinism 123\nsecond line", 20.0, 200.0, 300.0);
        let first = TextLayout::layout(&style);
        let second = TextLayout::layout(&style);
        assert_eq!(first.lines, second.lines);
        assert_eq!(first.size, second.size);
        if fonts_available() {
            assert_eq!(
                text_image(&style).expect("a valid style rasterizes"),
                text_image(&style).expect("a valid style rasterizes")
            );
        }
    }

    #[test]
    fn drawn_glyphs_land_where_the_layout_places_them() {
        if !fonts_available() {
            return;
        }
        let mut style = boxed("T", 72.0, 200.0, 120.0);
        style.red = 1.0;
        let layout = TextLayout::layout(&style);
        let glyph = layout.lines[0].glyphs[0].clone();
        let image = text_image(&style).expect("a valid style rasterizes");
        // The glyph's ink sits in the line, to the right of the padding, and its ink starts at the
        // left edge of its advance box.
        let ink_x: Vec<usize> = (0..image.width())
            .filter(|x| (0..image.height()).any(|y| image.get(*x, y)[3] > 0))
            .collect();
        let ink_y: Vec<usize> = (0..image.height())
            .filter(|y| (0..image.width()).any(|x| image.get(x, *y)[3] > 0))
            .collect();
        assert!(!ink_x.is_empty() && !ink_y.is_empty());
        let first_x = *ink_x.first().expect("ink") as f64;
        let last_x = *ink_x.last().expect("ink") as f64;
        let first_y = *ink_y.first().expect("ink") as f64;
        let last_y = *ink_y.last().expect("ink") as f64;
        // The glyph's ink sits inside its own advance box, which the layout put inside the box.
        assert!(glyph.x >= layout.padding, "the glyph starts inside the padding");
        assert!(
            first_x >= glyph.x - 1.0 && first_x < glyph.x + glyph.advance,
            "the ink starts inside the glyph's box: {first_x} vs {}",
            glyph.x
        );
        assert!(last_x < glyph.x + glyph.advance + 1.0, "the ink ends inside the glyph's box");
        // A capital sits on the line, above the baseline, and is as tall as the font size at most.
        assert!(first_y >= layout.lines[0].rect.min_y(), "the ink is inside the line");
        assert!(last_y < layout.lines[0].baseline, "a capital has no descender below the baseline");
        assert!(
            last_y >= layout.lines[0].baseline - layout.font_size as f64,
            "the ink is on the line"
        );
    }
}
