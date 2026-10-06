//! The canvas's line overlays: the layout grid, the guides, the marching ants, the lasso draft and
//! the snap guides — everything the Swift drew as lines over the composite, above the pixels and
//! below the transform box.
//!
//! Ported from the line-drawing parts of `Rendering/TransformOverlay.swift` (`drawLayoutGrid`,
//! `drawGuides`, `drawSelection`, `drawLassoDraft`, `drawSnapGuides`; the ants' out-of-focus level
//! of detail is noted in [`paint_selection`]) and from `CanvasLinesOverlay`, whose pixel grid this
//! port draws into the canvas raster instead (`Composite::draw_pixel_grid`, so it lands in the same
//! cached image the pixels do).
//!
//! gpui has no `CGContext`, so paths are flattened (`compositor_rs_core::path::flatten`, the same
//! 0.1-point tolerance the scanline filler uses) and stroked as polylines; dash patterns are walked
//! by hand because `PathBuilder::dash_array` has no phase.

use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::guides::{CanvasGuide, CanvasGuideAxis};
use compositor_rs_core::path::{flatten, Path};
use compositor_rs_core::selection::{LassoDraft, LassoKind};
use compositor_rs_core::viewport::CanvasViewport;
use compositor_rs_session::guides::{guide_color, GUIDE_ALPHA};
use compositor_rs_session::EditorSession;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// gpui's own point, in window pixels: `Point` below is `compositor_rs_core::geom::Point` (view and
// document space, CGFloat points), so the two are spelled apart.
use gpui_kit::Point as WindowPoint;

use crate::canvas::overlays::palette_rgba_alpha;

/// The ants' dash (`lengths: [4, 4]`), the pattern's width, and how far each tick steps it.
const ANTS_DASH: [f32; 2] = [4.0, 4.0];
/// The marching-ants timer's period (`Timer(timeInterval: 0.12)`), and the cycle it steps through.
const ANTS_PERIOD: f64 = 0.12;
const ANTS_CYCLE: f64 = 8.0;
/// The lasso draft's strokes (`lineWidth: 2` black at 0.8, then `lineWidth: 1` white).
const LASSO_OUTLINE_WIDTH: f32 = 2.0;
const LASSO_OUTLINE_ALPHA: f32 = 0.8;
const LASSO_INNER_WIDTH: f32 = 1.0;
/// The polygonal first corner's handle (`CGRect(x: first.x - 4, …, width: 8, height: 8)`).
const LASSO_HANDLE: f64 = 8.0;

/// The canvas's line overlays, drawn in one element above the composite.
///
/// `text_box_rect` is the box the Type tool is dragging out (`CanvasView.textBoxRect`, document
/// pixels): view state the canvas passes in, as the Swift's `drawLines(in:)` read it.
pub fn canvas_lines(session: Entity<EditorSession>, text_box_rect: Option<Rect>) -> CanvasLines {
    CanvasLines {
        session,
        text_box_rect,
    }
}

#[derive(IntoElement)]
pub struct CanvasLines {
    session: Entity<EditorSession>,
    text_box_rect: Option<Rect>,
}

impl RenderOnce for CanvasLines {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let session = self.session;
        // The Swift ticks `antsPhase` from a 0.12 s timer while a selection is up; the phase here is
        // that same step count off the monotonic clock, and each frame asks for the next so the
        // dashes march. The composite underneath stays cached, so only the overlays repaint.
        let clock = window.use_keyed_state(
            ("canvas-ants-clock", session.entity_id()),
            cx,
            |_, cx| AntsClock {
                started: cx.background_executor().now(),
            },
        );
        let grid = {
            let session = session.read(cx);
            session.document.as_ref().and_then(|document| {
                session.shows_grid.then(|| {
                    let size = document.size();
                    GridSnapshot {
                        layout: session.layout_grid,
                        color: session.grid_color(),
                        subdivision_alpha: session.grid_subdivision_alpha(),
                        major_alpha: session.grid_major_alpha(),
                        dashes: session.grid_dashes().to_vec(),
                        lines_x: session.layout_grid_lines(size.width),
                        lines_y: session.layout_grid_lines(size.height),
                    }
                })
            })
        };
        let guides = {
            let session = session.read(cx);
            if session.shows_guides {
                let guides = session.displayed_guides();
                (!guides.is_empty()).then_some(guides)
            } else {
                None
            }
        };
        let selection = session
            .read(cx)
            .displayed_selection()
            .filter(|selection| !selection.is_empty())
            .map(|selection| selection.path);
        let lasso = session.read(cx).lasso_draft.clone();
        let snap_guides = {
            let session = session.read(cx);
            let (xs, ys) = session.snap_guides.clone();
            (!xs.is_empty() || !ys.is_empty()).then_some((xs, ys))
        };
        let accent = cx.theme().primary;
        let text_box_rect = self.text_box_rect;

        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                let session = session.read(cx);
                let Some(document_size) = session.document.as_ref().map(|document| document.size())
                else {
                    return;
                };
                let viewport = session.viewport;
                let origin = bounds.origin;
                let width = f64::from(bounds.size.width);
                let height = f64::from(bounds.size.height);

                if let Some(grid) = &grid {
                    paint_layout_grid(&viewport, document_size, grid, origin, window);
                }
                if let Some(guides) = &guides {
                    paint_guides(&viewport, document_size, guides, origin, width, height, window);
                }
                if let Some(path) = &selection {
                    let phase = {
                        let now = cx.background_executor().now();
                        let elapsed = now.duration_since(clock.read(cx).started).as_secs_f64();
                        ((elapsed / ANTS_PERIOD) % ANTS_CYCLE) as f32
                    };
                    paint_selection(&viewport, document_size, path, origin, phase, window);
                    window.request_animation_frame();
                }
                if let Some(draft) = &lasso {
                    paint_lasso_draft(&viewport, document_size, draft, origin, window);
                }
                if let Some((xs, ys)) = &snap_guides {
                    paint_snap_guides(&viewport, document_size, xs, ys, origin, accent, window);
                }
                // `drawLines(in:)`'s tail: `drawTextBoxDraft()`.
                if let Some(rect) = text_box_rect {
                    let scale = viewport.points_per_pixel();
                    let view_origin = viewport.view_point(rect.origin, document_size);
                    stroke_rect(
                        window,
                        origin,
                        Rect::new(
                            view_origin.x,
                            view_origin.y,
                            rect.width() * scale,
                            rect.height() * scale,
                        ),
                        1.0,
                        accent.into(),
                    );
                }
            },
        )
        .absolute()
        .size_full()
    }
}

/// The monotonic clock the ants' phase is measured from (`antsTimer`).
struct AntsClock {
    started: std::time::Instant,
}

/// Everything `drawLayoutGrid` reads off the session, captured at render time.
struct GridSnapshot {
    layout: compositor_rs_core::guides::LayoutGrid,
    color: PaletteColor,
    subdivision_alpha: f64,
    major_alpha: f64,
    dashes: Vec<f64>,
    lines_x: Vec<f64>,
    lines_y: Vec<f64>,
}

// MARK: - Drawing helpers shared with the transform overlay

/// `TransformOverlay.documentToView`: translation to the document's rect, then the view scale.
pub(crate) fn document_to_view(session: &EditorSession) -> Option<AffineTransform> {
    let document = session.document.as_ref()?;
    view_transform(&session.viewport, document.size())
}

/// A view point in the overlay's window coordinates.
pub(crate) fn view_to_window(origin: WindowPoint<Pixels>, value: Point) -> WindowPoint<Pixels> {
    gpui_kit::point(origin.x + px(value.x as f32), origin.y + px(value.y as f32))
}

/// A document point in the overlay's window coordinates (`CGPoint.applying(documentToView)`).
pub(crate) fn document_to_window(
    viewport: &CanvasViewport,
    document_size: Size,
    origin: WindowPoint<Pixels>,
    value: Point,
) -> WindowPoint<Pixels> {
    view_to_window(origin, viewport.view_point(value, document_size))
}

/// A view-space rect as gpui bounds.
pub(crate) fn window_bounds(origin: WindowPoint<Pixels>, rect: Rect) -> Bounds<Pixels> {
    Bounds::new(
        view_to_window(origin, rect.origin),
        gpui_kit::size(px(rect.width() as f32), px(rect.height() as f32)),
    )
}

/// A filled axis-aligned rect (`context.fill(rect)`).
pub(crate) fn fill_rect(window: &mut Window, origin: WindowPoint<Pixels>, rect: Rect, color: Rgba) {
    window.paint_quad(fill(window_bounds(origin, rect), color));
}

/// A straight line stroked at `width`.
pub(crate) fn stroke_segment(
    window: &mut Window,
    from: WindowPoint<Pixels>,
    to: WindowPoint<Pixels>,
    width: f32,
    color: Rgba,
) {
    let mut builder = PathBuilder::stroke(px(width));
    builder.move_to(from);
    builder.line_to(to);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// A polyline (or closed outline) stroked at `width`.
pub(crate) fn stroke_polyline(
    window: &mut Window,
    points: &[WindowPoint<Pixels>],
    closed: bool,
    width: f32,
    color: Rgba,
) {
    if points.len() < 2 {
        return;
    }
    let mut builder = PathBuilder::stroke(px(width));
    builder.add_polygon(points, closed);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// A polyline stroked with a dash pattern advanced by `phase` points (`setLineDash(phase:lengths:)`).
pub(crate) fn stroke_polyline_dashed(
    window: &mut Window,
    points: &[WindowPoint<Pixels>],
    closed: bool,
    width: f32,
    color: Rgba,
    dash: [f32; 2],
    phase: f32,
) {
    let mut vertices = points.to_vec();
    if closed && vertices.len() > 1 {
        vertices.push(vertices[0]);
    }
    if vertices.len() < 2 {
        return;
    }
    let (on, off) = (dash[0], dash[1]);
    let cycle = on + off;
    if !(cycle > 0.0) {
        stroke_polyline(window, &vertices, false, width, color);
        return;
    }
    // Where in the pattern the walk starts (the phase), and how much of the current run is left.
    let position = phase.rem_euclid(cycle);
    let mut drawing = position < on;
    let mut remaining_in_state = if drawing { on - position } else { cycle - position };
    let mut builder = PathBuilder::stroke(px(width));
    let mut emitted = false;
    for pair in vertices.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let dx = f32::from(end.x) - f32::from(start.x);
        let dy = f32::from(end.y) - f32::from(start.y);
        let length = (dx * dx + dy * dy).sqrt();
        if !(length > 0.0) {
            continue;
        }
        let (unit_x, unit_y) = (dx / length, dy / length);
        let mut walked = 0.0f32;
        while walked < length {
            let step = (length - walked).min(remaining_in_state.max(0.0));
            if drawing && step > 0.0 {
                let from = point(start.x + px(unit_x * walked), start.y + px(unit_y * walked));
                let to = point(
                    start.x + px(unit_x * (walked + step)),
                    start.y + px(unit_y * (walked + step)),
                );
                if !emitted {
                    builder.move_to(from);
                    emitted = true;
                }
                builder.line_to(to);
            }
            walked += step;
            remaining_in_state -= step;
            if remaining_in_state <= 1e-6 {
                drawing = !drawing;
                remaining_in_state = if drawing { on } else { off };
            }
        }
    }
    if emitted {
        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    }
}

/// A circle's outline as a polygon, as [`super::sample_ring`] draws its ring.
pub(crate) fn circle_points(center: WindowPoint<Pixels>, radius: f32) -> Vec<WindowPoint<Pixels>> {
    const STEPS: usize = 64;
    (0..STEPS)
        .map(|step| {
            let angle = std::f32::consts::TAU * step as f32 / STEPS as f32;
            point(
                center.x + px(radius * angle.cos()),
                center.y + px(radius * angle.sin()),
            )
        })
        .collect()
}

/// A filled circle (`fillEllipse(in:)`).
pub(crate) fn fill_circle(window: &mut Window, center: WindowPoint<Pixels>, radius: f32, color: Rgba) {
    let points = circle_points(center, radius);
    let mut builder = PathBuilder::fill();
    builder.add_polygon(&points, true);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// A stroked circle (`strokeEllipse(in:)`).
pub(crate) fn stroke_circle(
    window: &mut Window,
    center: WindowPoint<Pixels>,
    radius: f32,
    width: f32,
    color: Rgba,
) {
    stroke_polyline(window, &circle_points(center, radius), true, width, color);
}

/// A view-space rect's outline stroked at `width`.
pub(crate) fn stroke_rect(
    window: &mut Window,
    origin: WindowPoint<Pixels>,
    rect: Rect,
    width: f32,
    color: Rgba,
) {
    let bounds = window_bounds(origin, rect);
    let points = [
        bounds.origin,
        bounds.top_right(),
        bounds.bottom_right(),
        bounds.bottom_left(),
    ];
    stroke_polyline(window, &points, true, width, color);
}

// MARK: - The Swift draw methods

/// `TransformOverlay.drawLayoutGrid`: majors in the chosen style, dotted subdivisions, both in the
/// chosen color.
fn paint_layout_grid(
    viewport: &CanvasViewport,
    document_size: Size,
    grid: &GridSnapshot,
    origin: WindowPoint<Pixels>,
    window: &mut Window,
) {
    let scale = viewport.points_per_pixel();
    let denominator = scale.max(0.0001);
    let hairline = 1.0 / viewport.backing_scale.max(1.0);
    // `setLineWidth(hairline / max(scale, 0.0001))` inside the document-space transform, in view
    // points: the context multiplies every length by the transform's scale.
    let line_width = (hairline / denominator * scale) as f32;
    let subdivision_gap = grid.layout.step() * scale;

    if subdivision_gap >= 4.0 {
        let dash = [
            (1.0 / denominator * scale) as f32,
            (2.0 / denominator * scale) as f32,
        ];
        let stroke = palette_rgba_alpha(grid.color, grid.subdivision_alpha as f32);
        for x in grid
            .lines_x
            .iter()
            .copied()
            .filter(|x| !grid.layout.is_major(*x))
        {
            stroke_polyline_dashed(
                window,
                &grid_segment(viewport, document_size, origin, x, true),
                false,
                line_width,
                stroke,
                dash,
                0.0,
            );
        }
        for y in grid
            .lines_y
            .iter()
            .copied()
            .filter(|y| !grid.layout.is_major(*y))
        {
            stroke_polyline_dashed(
                window,
                &grid_segment(viewport, document_size, origin, y, false),
                false,
                line_width,
                stroke,
                dash,
                0.0,
            );
        }
    }

    let stroke = palette_rgba_alpha(grid.color, grid.major_alpha as f32);
    let dash = [
        (grid.dashes.first().copied().unwrap_or(0.0) / denominator * scale) as f32,
        (grid.dashes.get(1).copied().unwrap_or(0.0) / denominator * scale) as f32,
    ];
    let dashed = grid.dashes.len() >= 2;
    for x in grid
        .lines_x
        .iter()
        .copied()
        .filter(|x| grid.layout.is_major(*x))
    {
        let segment = grid_segment(viewport, document_size, origin, x, true);
        if dashed {
            stroke_polyline_dashed(window, &segment, false, line_width, stroke, dash, 0.0);
        } else {
            stroke_polyline(window, &segment, false, line_width, stroke);
        }
    }
    for y in grid
        .lines_y
        .iter()
        .copied()
        .filter(|y| grid.layout.is_major(*y))
    {
        let segment = grid_segment(viewport, document_size, origin, y, false);
        if dashed {
            stroke_polyline_dashed(window, &segment, false, line_width, stroke, dash, 0.0);
        } else {
            stroke_polyline(window, &segment, false, line_width, stroke);
        }
    }
}

/// One grid line across the document, in window coordinates: `along_x` for a vertical line.
fn grid_segment(
    viewport: &CanvasViewport,
    document_size: Size,
    origin: WindowPoint<Pixels>,
    value: f64,
    along_x: bool,
) -> [WindowPoint<Pixels>; 2] {
    if along_x {
        [
            document_to_window(viewport, document_size, origin, Point::new(value, 0.0)),
            document_to_window(
                viewport,
                document_size,
                origin,
                Point::new(value, document_size.height),
            ),
        ]
    } else {
        [
            document_to_window(viewport, document_size, origin, Point::new(0.0, value)),
            document_to_window(
                viewport,
                document_size,
                origin,
                Point::new(document_size.width, value),
            ),
        ]
    }
}

/// `TransformOverlay.drawGuides`: user guides span the whole view, including the pasteboard.
fn paint_guides(
    viewport: &CanvasViewport,
    document_size: Size,
    guides: &[CanvasGuide],
    origin: WindowPoint<Pixels>,
    width: f64,
    height: f64,
    window: &mut Window,
) {
    let color = palette_rgba_alpha(guide_color(), GUIDE_ALPHA as f32);
    let line_width = (1.0 / viewport.backing_scale.max(1.0)) as f32;
    for guide in guides {
        let segment = if guide.axis == CanvasGuideAxis::Vertical {
            let x = viewport
                .view_point(Point::new(guide.position, 0.0), document_size)
                .x;
            [
                view_to_window(origin, Point::new(x, 0.0)),
                view_to_window(origin, Point::new(x, height)),
            ]
        } else {
            let y = viewport
                .view_point(Point::new(0.0, guide.position), document_size)
                .y;
            [
                view_to_window(origin, Point::new(0.0, y)),
                view_to_window(origin, Point::new(width, y)),
            ]
        };
        stroke_polyline(window, &segment, false, line_width, color);
    }
}

/// `TransformOverlay.drawSelection`: a white line under an animated black dash.
///
/// The Swift traces a screen-resolution copy of an outline over 20 000 elements so a zoomed-out wand
/// selection does not bog the redraw down; that level of detail caches itself out of band, which
/// this stateless element cannot, so every outline is stroked at full detail here.
fn paint_selection(
    viewport: &CanvasViewport,
    document_size: Size,
    path: &Path,
    origin: WindowPoint<Pixels>,
    phase: f32,
    window: &mut Window,
) {
    let Some(transform) = view_transform(viewport, document_size) else {
        return;
    };
    let subpaths = flatten(path, &transform);
    let white: Rgba = gpui_kit::white().into();
    let black: Rgba = gpui_kit::black().into();
    let polylines: Vec<Vec<WindowPoint<Pixels>>> = subpaths
        .iter()
        .map(|subpath| {
            subpath
                .points
                .iter()
                .map(|value| view_to_window(origin, *value))
                .collect()
        })
        .collect();
    for (polyline, subpath) in polylines.iter().zip(subpaths.iter()) {
        stroke_polyline(window, polyline, subpath.closed, 1.0, white);
    }
    for (polyline, subpath) in polylines.iter().zip(subpaths.iter()) {
        stroke_polyline_dashed(window, polyline, subpath.closed, 1.0, black, ANTS_DASH, phase);
    }
}

/// The document → view map of a viewport (the `documentToView` transform).
pub(crate) fn view_transform(
    viewport: &CanvasViewport,
    document_size: Size,
) -> Option<AffineTransform> {
    let origin = viewport.document_rect(document_size).origin;
    let scale = viewport.points_per_pixel();
    Some(AffineTransform::translation(origin.x, origin.y).scaled_by(scale, scale))
}

/// `TransformOverlay.drawLassoDraft`: the rubber band, its ellipse and its first-corner handle.
fn paint_lasso_draft(
    viewport: &CanvasViewport,
    document_size: Size,
    draft: &LassoDraft,
    origin: WindowPoint<Pixels>,
    window: &mut Window,
) {
    let Some(transform) = view_transform(viewport, document_size) else {
        return;
    };
    let mut points: Vec<Point> = draft
        .points
        .iter()
        .map(|value| transform.applying(*value))
        .collect();
    if draft.kind == LassoKind::Polygonal {
        if let Some(cursor) = draft.cursor {
            points.push(transform.applying(cursor));
        }
    }
    let Some(first) = points.first().copied() else {
        return;
    };
    let black = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: LASSO_OUTLINE_ALPHA,
    };
    let white: Rgba = gpui_kit::white().into();
    let outer: Vec<WindowPoint<Pixels>>;
    let inner: Vec<WindowPoint<Pixels>>;
    if draft.kind == LassoKind::Ellipse && points.len() == 4 {
        let min_x = points.iter().fold(f64::INFINITY, |value, point| value.min(point.x));
        let max_x = points.iter().fold(f64::NEG_INFINITY, |value, point| value.max(point.x));
        let min_y = points.iter().fold(f64::INFINITY, |value, point| value.min(point.y));
        let max_y = points.iter().fold(f64::NEG_INFINITY, |value, point| value.max(point.y));
        let center = view_to_window(
            origin,
            Point::new((min_x + max_x) / 2.0, (min_y + max_y) / 2.0),
        );
        let radius_x = ((max_x - min_x) / 2.0) as f32;
        let radius_y = ((max_y - min_y) / 2.0) as f32;
        const STEPS: usize = 128;
        let ellipse: Vec<WindowPoint<Pixels>> = (0..STEPS)
            .map(|step| {
                let angle = std::f32::consts::TAU * step as f32 / STEPS as f32;
                point(
                    center.x + px(radius_x * angle.cos()),
                    center.y + px(radius_y * angle.sin()),
                )
            })
            .collect();
        outer = ellipse.clone();
        inner = ellipse;
    } else {
        outer = points.iter().map(|value| view_to_window(origin, *value)).collect();
        inner = outer.clone();
    }
    let closed = draft.kind == LassoKind::Rectangle || draft.kind == LassoKind::Ellipse;
    stroke_polyline(window, &outer, closed, LASSO_OUTLINE_WIDTH, black);
    stroke_polyline(window, &inner, closed, LASSO_INNER_WIDTH, white);
    if draft.kind == LassoKind::Polygonal {
        // The first corner: click it to close the outline.
        let handle = Rect::new(
            first.x - LASSO_HANDLE / 2.0,
            first.y - LASSO_HANDLE / 2.0,
            LASSO_HANDLE,
            LASSO_HANDLE,
        );
        fill_rect(window, origin, handle, white);
        stroke_rect(window, origin, handle, LASSO_INNER_WIDTH, black);
    }
}

/// `TransformOverlay.drawSnapGuides`: while a move is snapped, a line along what it lined up with.
fn paint_snap_guides(
    viewport: &CanvasViewport,
    document_size: Size,
    xs: &[f64],
    ys: &[f64],
    origin: WindowPoint<Pixels>,
    accent: Hsla,
    window: &mut Window,
) {
    let color: Rgba = accent.into();
    for x in xs {
        let segment = [
            document_to_window(viewport, document_size, origin, Point::new(*x, 0.0)),
            document_to_window(
                viewport,
                document_size,
                origin,
                Point::new(*x, document_size.height),
            ),
        ];
        stroke_polyline(window, &segment, false, 1.0, color);
    }
    for y in ys {
        let segment = [
            document_to_window(viewport, document_size, origin, Point::new(0.0, *y)),
            document_to_window(
                viewport,
                document_size,
                origin,
                Point::new(document_size.width, *y),
            ),
        ];
        stroke_polyline(window, &segment, false, 1.0, color);
    }
}
