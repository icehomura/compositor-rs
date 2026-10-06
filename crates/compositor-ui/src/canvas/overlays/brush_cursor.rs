//! The brush circle drawn over the canvas while a brush tool is active
//! (`Rendering/BrushCursorOverlay.swift`).
//!
//! The circle, the hardness ring and the Clone Stamp's source crosshair are painted here from the
//! session's pointer (`EditorSession.brush_pointer`) and brush settings.
//!
//! **Not yet wired.** Two of the Swift view's inputs live in `CanvasView`, not the session, and the
//! canvas does not publish them yet: the hover `brushPointer` (so the circle only follows a stroke
//! in progress, not the resting pointer) and `brushTipDrag?.hardnessShown` (so the hardness ring
//! waits for a caller of [`paint_hardness_ring`]). The Swift view also previews, inside the circle,
//! what a Clone Stamp click would stamp — the source pixels through one click's coverage at the
//! brush's opacity — which needs `EditorSession::draw_live_composite` and the brush engine's tip
//! coverage; neither is reachable from this slice yet. The marker, diameter, strokes and dash
//! lengths are the Swift ones.

use compositor_core::document::NavigationTool;
use compositor_core::geom::Point;
use compositor_core::viewport::CanvasViewport;
use compositor_session::EditorSession;

use gpui_kit::*;
// gpui's own point, in window pixels, as opposed to core's `Point` (view space, CGFloat points).
use gpui_kit::Point as WindowPoint;

use super::canvas_lines::{circle_points, fill_circle, stroke_circle, view_to_window};

/// The circle's white under-stroke and black over-stroke (`lineWidth: 2.5`, then `1`).
const CIRCLE_OUTER_WIDTH: f32 = 2.5;
const CIRCLE_INNER_WIDTH: f32 = 1.0;
/// The hardness ring's dashes (`setLineDash(lengths: [4, 3])`).
const HARDNESS_DASH: [f32; 2] = [4.0, 3.0];
/// The Clone Stamp crosshair's arms (`markerReach: 7`) and strokes (white 3, then black 1).
const MARKER_REACH: f32 = 7.0;
const MARKER_OUTER_WIDTH: f32 = 3.0;
const MARKER_INNER_WIDTH: f32 = 1.0;

/// The brush circle, drawn while a brush tool shows it. `hardness` is
/// `brushTipDrag?.hardnessShown == true ? session.brushSettings.hardness : nil` — view state the
/// canvas passes in.
pub fn brush_cursor(session: Entity<EditorSession>, hardness: Option<f64>) -> BrushCursor {
    BrushCursor { session, hardness }
}

#[derive(IntoElement)]
pub struct BrushCursor {
    session: Entity<EditorSession>,
    hardness: Option<f64>,
}

impl RenderOnce for BrushCursor {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let session = self.session;
        let hardness = self.hardness;
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
                // `updateBrushCursor`: `session.tool.isBrushTool && !spaceHeld && !picking &&
                // middlePanPoint == nil`. Space-held and middle-pan are the canvas view's own
                // state, so only the tool, picking and busy checks live here.
                let shows = session.tool.is_brush_tool()
                    && !crate::canvas::canvas_view::picking(&session)
                    && !session.is_project_busy
                    && !session.is_importing;
                let pointer = session.brush_pointer;
                let diameter = {
                    let diameter = session
                        .brush_stroke
                        .as_ref()
                        .map(|stroke| stroke.settings.diameter)
                        .unwrap_or(session.brush_settings.diameter);
                    (diameter * viewport.points_per_pixel()).max(1.0)
                };
                let sample = if shows && session.tool == NavigationTool::CloneStamp {
                    pointer.and_then(|pointer| {
                        let point = viewport.document_point(pointer, document_size);
                        session
                            .clone_sample_point(point)
                            .map(|source| viewport.view_point(source, document_size))
                    })
                } else {
                    None
                };

                if shows {
                    if let Some(pointer) = pointer {
                        let center = view_to_window(origin, pointer);
                        let radius = (diameter / 2.0) as f32;
                        let white: Rgba = gpui_kit::white().into();
                        let black: Rgba = gpui_kit::black().into();
                        stroke_circle(window, center, radius, CIRCLE_OUTER_WIDTH, white);
                        stroke_circle(window, center, radius, CIRCLE_INNER_WIDTH, black);
                        if let Some(hardness) = hardness {
                            paint_hardness_ring(window, center, radius, hardness);
                        }
                    }
                }
                if let Some(sample) = sample {
                    let center = view_to_window(origin, sample);
                    let white: Rgba = gpui_kit::white().into();
                    let black: Rgba = gpui_kit::black().into();
                    stroke_marker(window, center, MARKER_OUTER_WIDTH, white);
                    stroke_marker(window, center, MARKER_INNER_WIDTH, black);
                }
            },
        )
        .absolute()
        .size_full()
    }
}

/// The hardness ring: the fraction of the radius painted at full strength, dashed as the Swift
/// drew it (`circle.insetBy(dx: circle.width * (1 - hardness) / 2, …)`).
///
/// The Swift showed it only while the right-button tip drag ran with Shift held
/// (`brushTipDrag?.hardnessShown == true ? session.brushSettings.hardness : nil`); that flag is
/// `CanvasView`'s own state and the session does not carry it yet, so the canvas calls this from
/// its `updateBrushCursor` once it does (see the module note).
pub fn paint_hardness_ring(
    window: &mut Window,
    center: WindowPoint<Pixels>,
    radius: f32,
    hardness: f64,
) {
    if !(hardness > 0.0) {
        return;
    }
    let white: Rgba = gpui_kit::white().into();
    let black: Rgba = gpui_kit::black().into();
    let ring = circle_points(center, (radius as f64 * hardness) as f32);
    super::canvas_lines::stroke_polyline_dashed(
        window,
        &ring,
        true,
        CIRCLE_OUTER_WIDTH,
        white,
        HARDNESS_DASH,
        0.0,
    );
    super::canvas_lines::stroke_polyline_dashed(
        window,
        &ring,
        true,
        CIRCLE_INNER_WIDTH,
        black,
        HARDNESS_DASH,
        0.0,
    );
}

/// The Clone Stamp crosshair: two arms with round caps (`setLineCap(.round)`), drawn at `width`.
fn stroke_marker(window: &mut Window, center: WindowPoint<Pixels>, width: f32, color: Rgba) {
    let reach = px(MARKER_REACH);
    let cap = width / 2.0;
    let arms = [
        (
            point(center.x - reach, center.y),
            point(center.x + reach, center.y),
        ),
        (
            point(center.x, center.y - reach),
            point(center.x, center.y + reach),
        ),
    ];
    for (from, to) in arms {
        super::canvas_lines::stroke_polyline(window, &[from, to], false, width, color);
        // Round caps: a half-disc at each end, as `setLineCap(.round)` draws them.
        fill_circle(window, from, cap, color);
        fill_circle(window, to, cap, color);
    }
}

/// The circle's diameter for a pointer, in view points (shared with any caller that needs it).
pub fn brush_diameter(session: &EditorSession, viewport: &CanvasViewport) -> f64 {
    let diameter = session
        .brush_stroke
        .as_ref()
        .map(|stroke| stroke.settings.diameter)
        .unwrap_or(session.brush_settings.diameter);
    (diameter * viewport.points_per_pixel()).max(1.0)
}
