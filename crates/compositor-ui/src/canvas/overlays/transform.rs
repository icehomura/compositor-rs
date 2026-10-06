//! The Move tool's box, the Crop frame and the Gradient tool's line
//! (`Rendering/TransformOverlay.swift`'s `TransformOverlayGeometry` and the parts of
//! `TransformOverlay` they draw — the grid, guides, ants, lasso and snap lines are
//! [`super::canvas_lines`]'s).
//!
//! The overlay is a separate element above the lines, so selecting a layer repaints shapes, not
//! image pixels. Its geometry and hit-testing are public: the canvas asks
//! [`TransformOverlayGeometry::hit`] before it starts a transform drag, exactly as the Swift
//! `CanvasView` did.

use compositor_core::document::{ImageLayer, NavigationTool};
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::image_ops::GradientShape;
use compositor_core::layer_transform::{LayerTransform, TransformDragMode};
use compositor_core::viewport::CanvasViewport;
use compositor_session::EditorSession;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;
// gpui's own point, in window pixels, as opposed to core's `Point` (view space, CGFloat points).
use gpui_kit::Point as WindowPoint;

use super::canvas_lines::{
    circle_points, fill_circle, fill_rect, stroke_circle, stroke_polyline, stroke_polyline_dashed,
    stroke_rect, view_to_window, window_bounds,
};
use super::gray_rgba;

/// How far past a handle's center a hit still counts (`near(_:)`'s 10 points).
const HIT_REACH: f64 = 10.0;
/// How far the rotation knob sits from the top-middle handle (`sin(radians) * 28`).
const ROTATION_REACH: f64 = 28.0;
/// A handle's square (`CGRect(… width: 7, height: 7)`) and the rotation knob's diameter (`8`).
const HANDLE_SIZE: f64 = 7.0;
const ROTATION_KNOB: f32 = 8.0;
/// The crop frame's third lines (`for index in 1...2 { fraction = index / 3 }`).
const CROP_THIRDS: [f64; 2] = [1.0 / 3.0, 2.0 / 3.0];
/// The crop handles' squares (`CGRect(… width: 8, height: 8)`).
const CROP_HANDLE: f64 = 8.0;
/// How far a crop edge or corner grabs (`let radius: CGFloat = 10`).
const CROP_REACH: f64 = 10.0;
/// The crop dimming and its third lines (`black.withAlphaComponent(0.6)`, `white…0.4`).
const CROP_DIM_ALPHA: f32 = 0.6;
const CROP_LINE_ALPHA: f32 = 0.4;
/// The gradient line's strokes (`black 0.7` at 3, then `white` at 1) and its end discs (12 across).
const GRADIENT_LINE_ALPHA: f32 = 0.7;
const GRADIENT_LINE_WIDTH: f32 = 3.0;
const GRADIENT_DISC: f32 = 12.0;
/// The radial rim's dashes (`setLineDash(lengths: [4, 4])`).
const GRADIENT_RIM_DASH: [f32; 2] = [4.0, 4.0];

/// `TransformOverlayGeometry`: where the box's handles sit, in view points.
#[derive(Clone, Debug, PartialEq)]
pub struct TransformOverlayGeometry {
    pub handles: Vec<Point>,
    pub rotation_handle: Point,
    /// A distortion has no single rotation, so its rotation handle is hidden.
    pub shows_rotation: bool,
}

impl TransformOverlayGeometry {
    pub fn new(transform: &LayerTransform, viewport: &CanvasViewport, document_size: Size) -> Self {
        let handles: Vec<Point> = LayerTransform::HANDLES
            .iter()
            .map(|unit| viewport.view_point(transform.point(*unit), document_size))
            .collect();
        let rotation_handle = Point::new(
            handles[1].x + transform.radians().sin() * ROTATION_REACH,
            handles[1].y - transform.radians().cos() * ROTATION_REACH,
        );
        Self {
            handles,
            rotation_handle,
            shows_rotation: true,
        }
    }

    /// Handles for a distortion: its four corners (document pixels) and the midpoints of its edges.
    pub fn with_corners(corners: &[Point], viewport: &CanvasViewport, document_size: Size) -> Self {
        let view: Vec<Point> = corners
            .iter()
            .map(|corner| viewport.view_point(*corner, document_size))
            .collect();
        let middle = |a: Point, b: Point| Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        let handles = vec![
            view[0],
            middle(view[0], view[1]),
            view[1],
            middle(view[1], view[2]),
            view[2],
            middle(view[2], view[3]),
            view[3],
            middle(view[3], view[0]),
        ];
        Self {
            rotation_handle: handles[1],
            handles,
            shows_rotation: false,
        }
    }

    /// The handle at `point`, in view points (`hit(_:)`).
    pub fn hit(&self, point: Point) -> Option<TransformDragMode> {
        let near =
            |other: Point| (point.x - other.x).hypot(point.y - other.y) <= HIT_REACH;
        if self.shows_rotation && near(self.rotation_handle) {
            return Some(TransformDragMode::Rotate);
        }
        if let Some(index) = self.handles.iter().position(|handle| near(*handle)) {
            return Some(TransformDragMode::Resize(index));
        }
        for (start, end, handle) in [(0usize, 2usize, 1usize), (2, 4, 3), (4, 6, 5), (6, 0, 7)] {
            let a = self.handles[start];
            let b = self.handles[end];
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            let length_squared = dx * dx + dy * dy;
            if !(length_squared > 0.0) {
                continue;
            }
            let t = ((point.x - a.x) * dx + (point.y - a.y) * dy) / length_squared;
            if (0.0..=1.0).contains(&t)
                && (point.x - a.x - t * dx).hypot(point.y - a.y - t * dy) <= HIT_REACH
            {
                return Some(TransformDragMode::Resize(handle));
            }
        }
        None
    }

    /// The resize arrows for the edge or corner a handle turns, turned with the box
    /// (`resizeCursor(for:)`).
    pub fn resize_cursor(&self, index: usize) -> CursorStyle {
        // `.right`, `.bottomRight`, `.bottom`, `.topRight` with inward and outward directions.
        const POSITIONS: [CursorStyle; 4] = [
            CursorStyle::ResizeLeftRight,
            CursorStyle::ResizeUpLeftDownRight,
            CursorStyle::ResizeUpDown,
            CursorStyle::ResizeUpRightDownLeft,
        ];
        let quarter = std::f64::consts::PI / 4.0;
        let offsets: [f64; 8] = [quarter, 2.0 * quarter, 3.0 * quarter, 0.0, quarter, 2.0 * quarter, 3.0 * quarter, 0.0];
        let angle =
            (self.handles[2].y - self.handles[0].y).atan2(self.handles[2].x - self.handles[0].x);
        let direction = (((angle + offsets[index]) / quarter).round() as i64).rem_euclid(4) as usize;
        POSITIONS[direction]
    }
}

/// The Move tool's overlay: the box, the Crop frame, or the Gradient line — whichever the tool shows.
pub fn transform_overlay(session: Entity<EditorSession>) -> TransformOverlayView {
    TransformOverlayView { session }
}

#[derive(IntoElement)]
pub struct TransformOverlayView {
    session: Entity<EditorSession>,
}

impl RenderOnce for TransformOverlayView {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let session = self.session;
        let accent = cx.theme().primary;
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
                let view_size = Size::new(
                    f64::from(bounds.size.width),
                    f64::from(bounds.size.height),
                );
                // `draw()`'s chain: the crop frame, or the gradient line, or the transform box.
                if session.tool == NavigationTool::Crop {
                    paint_crop(&session, view_size, origin, window);
                } else if let Some(line) = gradient_line(&session) {
                    paint_gradient_line(&session, line, origin, window);
                } else if let Some(geometry) = geometry(&session) {
                    paint_transform_handles(&geometry, origin, accent, window);
                }
            },
        )
        .absolute()
        .size_full()
    }
}

/// `TransformOverlay.geometry`: the box the Move tool is showing, if any.
pub fn geometry(session: &EditorSession) -> Option<TransformOverlayGeometry> {
    if session.tool != NavigationTool::Move
        || !(session.shows_transform_controls
            || session
                .transform_edit
                .as_ref()
                .is_some_and(|edit| edit.persistent))
    {
        return None;
    }
    let document = session.document.as_ref()?;
    let document_size = document.size();
    // Several layers selected, or a folder: one box around them all.
    if session
        .transform_edit
        .as_ref()
        .is_some_and(|edit| edit.group.is_some())
        || (session.transform_edit.is_none() && session.transforms_as_group())
    {
        if let Some(corners) = session
            .transform_edit
            .as_ref()
            .and_then(|edit| edit.corners.as_ref())
        {
            return Some(TransformOverlayGeometry::with_corners(
                corners,
                &session.viewport,
                document_size,
            ));
        }
        let box_ = session
            .transform_edit
            .as_ref()
            .map(|edit| edit.draft)
            .or_else(|| session.group_transform_box())?;
        return Some(TransformOverlayGeometry::new(
            &box_,
            &session.viewport,
            document_size,
        ));
    }
    let layer: &ImageLayer = session.active_layer()?;
    if layer.asset.is_none() || layer.is_group || !document.effective_visible_ids().contains(&layer.id)
    {
        return None;
    }
    if let Some(edit) = session
        .transform_edit
        .as_ref()
        .filter(|edit| edit.layer_id == layer.id)
    {
        if let Some(corners) = edit.corners.as_ref() {
            return Some(TransformOverlayGeometry::with_corners(
                corners,
                &session.viewport,
                document_size,
            ));
        }
    }
    Some(TransformOverlayGeometry::new(
        &session.edited_transform(layer),
        &session.viewport,
        document_size,
    ))
}

/// `TransformOverlay.gradientLine`: pending gradient endpoints in view coordinates.
pub fn gradient_line(session: &EditorSession) -> Option<(Point, Point)> {
    let edit = session.gradient_edit.as_ref()?;
    if !edit.has_line() {
        return None;
    }
    let document = session.document.as_ref()?;
    let document_size = document.size();
    Some((
        session.viewport.view_point(edit.start, document_size),
        session.viewport.view_point(edit.end, document_size),
    ))
}

/// `TransformOverlay.cropViewRect`: where the Crop tool's frame sits, in view points.
pub fn crop_view_rect(session: &EditorSession) -> Option<Rect> {
    let rect = session.visible_crop_rect()?;
    let document = session.document.as_ref()?;
    let origin = session.viewport.view_point(rect.origin, document.size());
    let scale = session.viewport.points_per_pixel();
    Some(Rect::new(
        origin.x,
        origin.y,
        rect.width() * scale,
        rect.height() * scale,
    ))
}

/// `TransformOverlay.cropHandles`: the eight handles on the crop frame, in view points.
pub fn crop_handles(session: &EditorSession) -> Vec<Point> {
    let Some(rect) = crop_view_rect(session) else {
        return Vec::new();
    };
    LayerTransform::HANDLES
        .iter()
        .map(|unit| {
            Point::new(
                rect.min_x() + unit.x * rect.width(),
                rect.min_y() + unit.y * rect.height(),
            )
        })
        .collect()
}

/// `TransformOverlay.cropResizeRegions`: the corner squares and the whole-edge bands that start a
/// crop resize, in the order the Swift listed them.
pub fn crop_resize_regions(session: &EditorSession) -> Vec<(usize, Rect)> {
    let Some(rect) = crop_view_rect(session) else {
        return Vec::new();
    };
    let handles = crop_handles(session);
    let radius = CROP_REACH;
    let mut regions: Vec<(usize, Rect)> = [0usize, 2, 4, 6]
        .into_iter()
        .map(|index| {
            (
                index,
                Rect::new(
                    handles[index].x - radius,
                    handles[index].y - radius,
                    radius * 2.0,
                    radius * 2.0,
                ),
            )
        })
        .collect();
    // Entire edges are draggable, not just the small midpoint squares.
    for index in [1usize, 5] {
        regions.push((
            index,
            Rect::new(
                rect.min_x() + radius,
                handles[index].y - radius,
                (rect.width() - radius * 2.0).max(0.0),
                radius * 2.0,
            ),
        ));
    }
    for index in [3usize, 7] {
        regions.push((
            index,
            Rect::new(
                handles[index].x - radius,
                rect.min_y() + radius,
                radius * 2.0,
                (rect.height() - radius * 2.0).max(0.0),
            ),
        ));
    }
    regions
}

/// `TransformOverlay.drawTransformHandles`: the accent box, the eight handles and the rotation knob.
fn paint_transform_handles(
    geometry: &TransformOverlayGeometry,
    origin: WindowPoint<Pixels>,
    accent: Hsla,
    window: &mut Window,
) {
    let accent: Rgba = accent.into();
    let white: Rgba = gpui_kit::white().into();
    let mut builder = PathBuilder::stroke(px(1.0));
    builder.move_to(view_to_window(origin, geometry.handles[0]));
    for index in [2usize, 4, 6] {
        builder.line_to(view_to_window(origin, geometry.handles[index]));
    }
    builder.close();
    if geometry.shows_rotation {
        builder.move_to(view_to_window(origin, geometry.handles[1]));
        builder.line_to(view_to_window(origin, geometry.rotation_handle));
    }
    // Just the accent line: a dark line behind it read as a grey halo around the box.
    if let Ok(path) = builder.build() {
        window.paint_path(path, accent);
    }
    for point in &geometry.handles {
        let rect = Rect::new(
            point.x - HANDLE_SIZE / 2.0,
            point.y - HANDLE_SIZE / 2.0,
            HANDLE_SIZE,
            HANDLE_SIZE,
        );
        fill_rect(window, origin, rect, white);
        stroke_rect(window, origin, rect, 1.0, accent);
    }
    if !geometry.shows_rotation {
        return;
    }
    let center = view_to_window(origin, geometry.rotation_handle);
    fill_circle(window, center, ROTATION_KNOB / 2.0, white);
    stroke_circle(window, center, ROTATION_KNOB / 2.0, 1.0, accent);
}

/// `TransformOverlay.drawGradientLine`: the drag line, the radial rim and the endpoint discs.
fn paint_gradient_line(
    session: &EditorSession,
    line: (Point, Point),
    origin: WindowPoint<Pixels>,
    window: &mut Window,
) {
    let (start, end) = line;
    let start_point = view_to_window(origin, start);
    let end_point = view_to_window(origin, end);
    let black = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    let white: Rgba = gpui_kit::white().into();
    if session.gradient_settings.shape == GradientShape::Radial {
        // Faint rim where the radial gradient reaches its end color.
        let radius = (end.x - start.x).hypot(end.y - start.y) as f32;
        let rim = circle_points(start_point, radius);
        stroke_polyline_dashed(
            window,
            &rim,
            true,
            2.0,
            Rgba { a: 0.5, ..black },
            GRADIENT_RIM_DASH,
            0.0,
        );
        stroke_polyline_dashed(
            window,
            &rim,
            true,
            1.0,
            Rgba { a: 0.8, ..white },
            GRADIENT_RIM_DASH,
            0.0,
        );
    }
    stroke_polyline(
        window,
        &[start_point, end_point],
        false,
        GRADIENT_LINE_WIDTH,
        Rgba {
            a: GRADIENT_LINE_ALPHA,
            ..black
        },
    );
    stroke_polyline(window, &[start_point, end_point], false, 1.0, white);
    let colors = session.gradient_colors(false);
    for (point, color) in [
        (start_point, colors.first().copied()),
        (end_point, colors.last().copied()),
    ] {
        let radius = GRADIENT_DISC / 2.0;
        fill_circle(window, point, radius, white);
        stroke_circle(window, point, radius, 1.0, black);
        // Checkerboard shows through transparent ends.
        let inner = radius - 2.5;
        fill_circle(window, point, inner, gray_rgba(0.75, 1.0));
        if let Some(color) = color {
            fill_circle(window, point, inner, srgb(color));
        }
    }
}

/// `TransformOverlay.drawCrop`: the dimmed surround, the third lines and the handles.
fn paint_crop(
    session: &EditorSession,
    view_size: Size,
    origin: WindowPoint<Pixels>,
    window: &mut Window,
) {
    let Some(rect) = crop_view_rect(session) else {
        return;
    };
    let black = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    let white: Rgba = gpui_kit::white().into();
    // `addRect(bounds); addRect(rect); drawPath(using: .eoFill)`.
    fill_even_odd(
        window,
        origin,
        Rect::new(0.0, 0.0, view_size.width, view_size.height),
        rect,
        Rgba {
            a: CROP_DIM_ALPHA,
            ..black
        },
    );
    stroke_rect(window, origin, rect, 1.0, white);
    let faint = Rgba {
        a: CROP_LINE_ALPHA,
        ..white
    };
    for fraction in CROP_THIRDS {
        let x = rect.min_x() + rect.width() * fraction;
        stroke_polyline(
            window,
            &[
                view_to_window(origin, Point::new(x, rect.min_y())),
                view_to_window(origin, Point::new(x, rect.max_y())),
            ],
            false,
            1.0,
            faint,
        );
        let y = rect.min_y() + rect.height() * fraction;
        stroke_polyline(
            window,
            &[
                view_to_window(origin, Point::new(rect.min_x(), y)),
                view_to_window(origin, Point::new(rect.max_x(), y)),
            ],
            false,
            1.0,
            faint,
        );
    }
    for point in crop_handles(session) {
        let handle = Rect::new(
            point.x - CROP_HANDLE / 2.0,
            point.y - CROP_HANDLE / 2.0,
            CROP_HANDLE,
            CROP_HANDLE,
        );
        fill_rect(window, origin, handle, white);
        stroke_rect(window, origin, handle, 1.0, black);
    }
}

/// An even-odd fill of `outer` minus `inner` (`drawPath(using: .eoFill)`).
fn fill_even_odd(
    window: &mut Window,
    origin: WindowPoint<Pixels>,
    outer: Rect,
    inner: Rect,
    color: Rgba,
) {
    let mut builder = PathBuilder::fill().with_style(PathStyle::Fill(
        FillOptions::default().with_fill_rule(FillRule::EvenOdd),
    ));
    let outer = window_bounds(origin, outer);
    let inner = window_bounds(origin, inner);
    builder.move_to(outer.origin);
    builder.line_to(outer.top_right());
    builder.line_to(outer.bottom_right());
    builder.line_to(outer.bottom_left());
    builder.close();
    builder.move_to(inner.origin);
    builder.line_to(inner.top_right());
    builder.line_to(inner.bottom_right());
    builder.line_to(inner.bottom_left());
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// A straight `[f64; 4]` sRGB color as gpui's [`Rgba`] (the gradient fill's own components).
fn srgb(color: [f64; 4]) -> Rgba {
    Rgba {
        r: color[0] as f32,
        g: color[1] as f32,
        b: color[2] as f32,
        a: color[3] as f32,
    }
}
