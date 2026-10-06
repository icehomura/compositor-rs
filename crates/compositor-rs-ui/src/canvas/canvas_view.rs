//! The canvas: the document drawn through the compositing pipeline, and every pointer, wheel and key
//! event the editor's tools read.
//!
//! Ported from `EditorCanvas`/`CanvasView` in `Rendering/EditorCanvas.swift`. The AppKit view is a
//! gpui element here: the composite is rasterized by [`compositor_rs_render::Composite::draw_view`] and
//! painted as an image, while the overlay layers (guides and marching ants, the transform box, the
//! brush cursor and the sample ring) are the sibling elements in [`crate::canvas::overlays`] stacked
//! over it.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::geom::{Point, Rect, Size};
use compositor_rs_core::guides::{CanvasGuide, CanvasGuideAxis};
use compositor_rs_core::layer_transform::{LayerTransform, TransformDrag, TransformDragMode, TransformSnap};
use compositor_rs_core::selection::{LassoKind, SelectionMode};
use compositor_rs_core::Rgba8Image;
use compositor_rs_io::image_exporter::{shared, ExportRaster, ImageExporter};
use compositor_rs_pixels::canvas::Canvas;
use compositor_rs_render::composite::{Composite, CompositeState};
use compositor_rs_session::crop::{CropDrag, CropDragMode, CropSnap};
use compositor_rs_session::projects::SessionHost;
use compositor_rs_session::EditorSession;

use crate::canvas::overlays::brush_cursor::brush_cursor;
use crate::canvas::overlays::canvas_lines::canvas_lines;
use crate::canvas::overlays::sample_ring::{sample_ring, SampleRingState};
use crate::canvas::overlays::transform::{crop_resize_regions, transform_overlay};

use gpui_kit::*;
// gpui's own point, in window pixels, as opposed to core's `Point` (view and document space, in
// points), so the two are spelled apart.
use gpui_kit::Point as WindowPoint;

/// How close, in screen points, a crop edge comes to a layer or canvas edge before it snaps
/// (`CanvasView.cropSnapDistance`).
pub const CROP_SNAP_DISTANCE: f64 = 8.0;

/// How close, in points, a guide comes before the pointer picks it up
/// (`EditorSession.guideHitDistance`; the same constant the session's own hit test defaults to).
pub const GUIDE_HIT_TOLERANCE: f64 = compositor_rs_core::snap::GUIDE_HIT_DISTANCE;

/// What the left button is dragging, from the press onward.
#[derive(Clone, Debug, PartialEq)]
enum Drag {
    /// Space or the Hand tool: the document follows the pointer.
    Pan,
    /// A brush-family stroke.
    Brush,
    /// A gradient's end being dragged.
    Gradient(GradientHandle),
    /// The Marquee, Lasso or Wand's draft.
    Selection,
    /// The Crop tool's rectangle.
    Crop(CropDrag),
    /// The Move tool's transform.
    Transform(TransformDrag),
    /// A guide pulled off a ruler.
    Guide,
    /// The Zoom tool: click zooms, dragging zooms smoothly.
    Zoom { start: Point, zoom: f64, moved: bool },
    /// A sampling drag from the color picker or a panel's eyedropper.
    Sampling,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum GradientHandle {
    Start,
    End,
}

/// The right button's brush-tip drag: left and right resize the brush, Shift changes its hardness
/// (`CanvasView.brushTipDrag`).
#[derive(Clone, Copy, Debug)]
struct BrushTipDrag {
    start: Point,
    diameter: f64,
    hardness: f64,
    hardness_shown: bool,
}

/// Everything a pointer event reads from the session before it changes anything, so the session is
/// never borrowed twice.
#[derive(Clone, Debug)]
struct Snap {
    has_document: bool,
    busy: bool,
    tool: NavigationTool,
    pixel: Point,
    points_per_pixel: f64,
    picking: bool,
    mode: SelectionMode,
    wand_object: bool,
    draft_kind: Option<LassoKind>,
    crop_ratio: Option<f64>,
    crop_snap_targets: (Vec<f64>, Vec<f64>),
    guide_hit: Option<CanvasGuide>,
    guide_axis: Option<CanvasGuideAxis>,
    locks_guides: bool,
    shows_guides: bool,
    transform_original: Option<LayerTransform>,
    transform_persistent: bool,
    locks_transform_ratio: bool,
    selection_move_origin: bool,
    zoom: f64,
    has_gradient: bool,
    gradient_start: Option<Point>,
    gradient_end: Option<Point>,
    text_draft: bool,
}

impl Snap {
    fn read(session: &EditorSession, point: Point) -> Self {
        let document_size = session.document.as_ref().map(|document| document.size());
        let pixel = document_size
            .map(|size| session.viewport.document_point(point, size))
            .unwrap_or(point);
        let transform_original = session.active_layer().map(|layer| {
            session
                .transform_edit
                .as_ref()
                .map(|edit| edit.draft)
                .unwrap_or_else(|| session.displayed_transform(layer))
        });
        let gradient = session.gradient_edit.as_ref();
        Self {
            has_document: session.document.is_some(),
            busy: session.is_project_busy || session.is_importing,
            tool: session.tool,
            pixel,
            points_per_pixel: session.viewport.points_per_pixel(),
            picking: picking(session),
            mode: session.selection_mode_choice,
            wand_object: session.wand_mode == compositor_rs_core::selection::WandMode::Object,
            draft_kind: session.lasso_draft.as_ref().map(|draft| draft.kind),
            crop_ratio: session.crop_ratio(),
            crop_snap_targets: session.crop_snap_targets(),
            guide_hit: session.hit_guide(point, GUIDE_HIT_TOLERANCE),
            guide_axis: session.guide_drag.map(|drag| drag.axis),
            locks_guides: session.locks_guides,
            shows_guides: session.shows_guides,
            transform_original,
            transform_persistent: session.transform_edit.as_ref().is_some_and(|edit| edit.persistent),
            locks_transform_ratio: session.locks_transform_ratio,
            selection_move_origin: session.selection_move_origin.is_some(),
            zoom: session.viewport.zoom(),
            has_gradient: gradient.is_some(),
            gradient_start: gradient.map(|edit| edit.start),
            gradient_end: gradient.map(|edit| edit.end),
            text_draft: session.text_draft.is_some(),
        }
    }
}

/// Whether a press or drag is sampling rather than editing (`CanvasView.picking`).
pub fn picking(session: &EditorSession) -> bool {
    session.color_picker.is_some()
        || session.hue_sample_mode.is_some()
        || session.levels.is_some()
        || session.color_range.is_some()
        || session.tool == NavigationTool::Eyedropper
        || session
            .filter_edit
            .as_ref()
            .is_some_and(|edit| edit.samples_white_balance() || edit.samples_point_color() || edit.samples_defringe())
}

/// The raster the canvas paints, remembered so a frame that changed nothing does not composite again.
struct ImageCache {
    key: Option<ImageKey>,
    image: Option<Arc<RenderImage>>,
}

#[derive(Clone, Copy, PartialEq)]
struct ImageKey {
    document: Option<compositor_rs_core::Id>,
    size: Size,
    zoom: f64,
    pan: Size,
    brush_revision: u64,
    mask_alone: Option<compositor_rs_core::Id>,
    shows_pixel_grid: bool,
}

/// The canvas element.
pub struct CanvasView {
    session: Entity<EditorSession>,
    /// The platform services the session needs (the project store, the renderer, the exporter).
    host: Arc<dyn SessionHost>,
    focus: FocusHandle,
    /// The canvas's own origin in the window, so window points become canvas-local ones.
    origin: Rc<Cell<WindowPoint<Pixels>>>,
    cache: Rc<RefCell<ImageCache>>,
    drag: Option<Drag>,
    space_held: bool,
    /// The pointer while a brush tool shows its circle (`brushPointer`).
    brush_pointer: Option<Point>,
    /// Where a space or Hand-tool pan last was (`lastDragPoint`).
    last_drag_point: Option<Point>,
    /// Where a middle-button pan last was (`middlePanPoint`).
    middle_pan_point: Option<Point>,
    /// The Shift line a stroke is kept on (`brushAxisAnchor`, `brushAxisHorizontal`).
    brush_axis_anchor: Option<Point>,
    brush_axis_horizontal: Option<bool>,
    /// Where the stroke last went, so Shift locks from there (`brushLastPixel`).
    brush_last_pixel: Option<Point>,
    brush_tip_drag: Option<BrushTipDrag>,
    /// Whether Shift squares the Marquee draft (`marqueeConstrainArmed`).
    marquee_constrain_armed: bool,
    /// The snapping a crop drag uses, built when it starts (`cropSnap`).
    crop_snap: Option<CropSnap>,
    /// The text box's anchor while the Type tool drags one out (`textBoxAnchor`).
    text_box_anchor: Option<Point>,
    /// The box being dragged out, in document pixels (`textBoxRect`).
    text_box_rect: Option<Rect>,
    /// Where a selection-outline drag grabbed the selection, in document pixels
    /// (`selectionDragStart`).
    selection_drag_start: Option<Point>,
    /// Where the sample ring sits while sampling, and the color it compares against
    /// (`samplingColor`, `samplingOriginal`).
    sampling_point: Option<Point>,
    sampling_original: PaletteColor,
    /// Whether an Option-drag has yet made its duplicate (`duplicatesTransformOnDrag`).
    duplicates_transform_on_drag: bool,
    /// The cursor the canvas asks for (`NSCursor`).
    cursor: CursorStyle,
    /// Whether the pointer is over the canvas, so the brush cursor only shows there.
    pointer_inside: bool,
    /// The `canvasFocusRequest` value last honored.
    last_focus_request: u64,
}

impl CanvasView {
    pub fn new(session: Entity<EditorSession>, host: Arc<dyn SessionHost>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            host,
            focus: cx.focus_handle(),
            origin: Rc::new(Cell::new(WindowPoint::default())),
            cache: Rc::new(RefCell::new(ImageCache {
                key: None,
                image: None,
            })),
            drag: None,
            space_held: false,
            brush_pointer: None,
            last_drag_point: None,
            middle_pan_point: None,
            brush_axis_anchor: None,
            brush_axis_horizontal: None,
            brush_last_pixel: None,
            brush_tip_drag: None,
            marquee_constrain_armed: true,
            crop_snap: None,
            text_box_anchor: None,
            text_box_rect: None,
            selection_drag_start: None,
            sampling_point: None,
            sampling_original: PaletteColor::BLACK,
            duplicates_transform_on_drag: false,
            cursor: CursorStyle::Arrow,
            pointer_inside: false,
            last_focus_request: 0,
        }
    }

    /// A window point in the canvas's own coordinates (`convert(event.locationInWindow, from: nil)`).
    fn canvas_point(&self, position: WindowPoint<Pixels>) -> Point {
        let origin = self.origin.get();
        Point::new(
            f64::from(f32::from(position.x - origin.x)),
            f64::from(f32::from(position.y - origin.y)),
        )
    }

    /// `brushPointer = point`: the view's own pointer, and the copy the overlays read off the
    /// session (`brush_cursor(session, …)` cannot see this view's fields).
    fn set_brush_pointer(&mut self, pointer: Option<Point>, cx: &mut Context<Self>) {
        self.brush_pointer = pointer;
        self.session.update(cx, |session, _| session.brush_pointer = pointer);
    }

    /// `beginTextGesture(at:event:)`: opens the live text under the pointer, or starts dragging a
    /// new box out.
    fn begin_text_gesture(&mut self, point: Point, cx: &mut Context<Self>) {
        let (pixel, target) = {
            let session = self.session.read(cx);
            let Some(document) = session.document.as_ref() else {
                return;
            };
            let pixel = session.viewport.document_point(point, document.size());
            let visible = document.effective_visible_ids();
            let target = document
                .layers
                .iter()
                .rev()
                .find(|layer| {
                    visible.contains(&layer.id)
                        && layer.live_text().is_some()
                        && layer.transform.contains(pixel)
                })
                .map(|layer| layer.id);
            (pixel, target)
        };
        // `guard session.finishText()`: a draft that could not be applied keeps the gesture off.
        if !self.session.update(cx, |session, _| session.finish_text()) {
            return;
        }
        if let Some(id) = target {
            self.session.update(cx, |session, _| {
                session.select_layer(Some(id));
                session.edit_active_text();
            });
        } else {
            self.text_box_anchor = Some(pixel);
            self.text_box_rect = Some(Rect::new(pixel.x, pixel.y, 0.0, 0.0));
        }
    }

    /// `dragTextGesture(to:)`: the box being dragged out, as a whole-pixel rectangle.
    fn drag_text_gesture(&mut self, point: Point, cx: &mut Context<Self>) {
        let Some(anchor) = self.text_box_anchor else {
            return;
        };
        let pixel = self.session.read_with(cx, |session, _| {
            session
                .document
                .as_ref()
                .map(|document| session.viewport.document_point(point, document.size()))
        });
        let Some(pixel) = pixel else {
            return;
        };
        self.text_box_rect = Some(compositor_rs_core::selection::DragBox::rect(
            anchor, pixel, false, false,
        ));
    }

    /// `finishTextGesture()`: a click makes point text, a drag a fixed box.
    fn finish_text_gesture(&mut self, cx: &mut Context<Self>) {
        let Some(rect) = self.text_box_rect else {
            return;
        };
        self.text_box_anchor = None;
        self.text_box_rect = None;
        self.session.update(cx, |session, _| {
            if rect.width() < 4.0 && rect.height() < 4.0 {
                session.begin_text(rect.origin, true);
            } else {
                session.begin_text_in(rect);
            }
        });
    }

    /// `mouseEntered`/`mouseExited`: whether the pointer is over the canvas, and the brush pointer
    /// that leaves with it.
    fn hover_changed(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        if *hovered == self.pointer_inside {
            return;
        }
        self.pointer_inside = *hovered;
        if !self.pointer_inside {
            self.set_brush_pointer(None, cx);
        }
    }

    /// `snappedCorner(_:flags:)`: whole document pixels, as a draft's corners are.
    fn snapped_corner(point: Point) -> Point {
        Point::new(point.x.round(), point.y.round())
    }

    /// `CanvasView.snapped(_:around:)`: the point on the 45° line through `around`.
    fn snapped(point: Point, around: Point) -> Point {
        let dx = point.x - around.x;
        let dy = point.y - around.y;
        if dx.abs() >= dy.abs() {
            Point::new(point.x, around.y)
        } else {
            Point::new(around.x, point.y)
        }
    }

    /// The cursor a tool wants over the canvas (`transformCursor(at:flags:)`, `lassoCursor`).
    fn tool_cursor(session: &EditorSession, space_held: bool, option: bool) -> CursorStyle {
        if space_held || session.tool == NavigationTool::Hand {
            return CursorStyle::OpenHand;
        }
        if session.is_project_busy || session.is_importing {
            return CursorStyle::Arrow;
        }
        if picking(session) {
            return CursorStyle::Crosshair;
        }
        match session.tool {
            // The Swift drew a magnifier here; gpui has no such cursor, so the zoom tool keeps the
            // toolkit's precision crosshair.
            NavigationTool::Zoom => CursorStyle::Crosshair,
            NavigationTool::Move => {
                if option {
                    // `NSCursor.duplicateCursor` (Option-drag duplicates): the toolkit's copy cursor.
                    CursorStyle::DragCopy
                } else {
                    // The Swift's four-way `moveCursor` has no gpui counterpart; the arrow it was
                    // drawn over is the closest the toolkit has.
                    CursorStyle::Arrow
                }
            }
            NavigationTool::Marquee
            | NavigationTool::Lasso
            | NavigationTool::Wand
            | NavigationTool::Crop
            | NavigationTool::Brush
            | NavigationTool::SpotHealing
            | NavigationTool::CloneStamp
            | NavigationTool::Blur
            | NavigationTool::Gradient
            | NavigationTool::Shape
            | NavigationTool::Type => CursorStyle::Crosshair,
            _ => CursorStyle::Arrow,
        }
    }

    /// The whole canvas raster, or the cached one when nothing it depends on changed.
    fn raster(
        session: &Entity<EditorSession>,
        size: Size,
        cache: &Rc<RefCell<ImageCache>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        let session = session.read(cx);
        let document = session.document.as_ref()?;
        if size.width < 1.0 || size.height < 1.0 {
            return None;
        }
        let key = ImageKey {
            document: Some(document.id),
            size,
            zoom: session.viewport.zoom(),
            pan: session.viewport.pan,
            brush_revision: session.brush_revision,
            mask_alone: session.mask_alone_layer().map(|layer| layer.id),
            shows_pixel_grid: session.shows_pixel_grid,
        };
        {
            let cache = cache.borrow();
            if cache.key == Some(key) {
                return cache.image.clone();
            }
        }
        let viewport = session.viewport;
        let mut target = Rgba8Image::new(size.width.max(1.0) as usize, size.height.max(1.0) as usize);
        let state = CompositeState {
            mask_alone: session.mask_alone_layer().map(|layer| layer.id),
            foreground: session.foreground_color(),
            shape_line_width: session.shape_line_width,
            active_layer: session.active_layer_id,
            ..CompositeState::default()
        };
        Composite::draw_view(document, &viewport, size, &mut target, &state);
        if session.pixel_grid_visible() {
            let mut canvas = Canvas::from_rgba(target);
            let view = Rect::new(0.0, 0.0, size.width, size.height);
            Composite::draw_pixel_grid(document, &viewport, view, &mut canvas);
            target = canvas.into_rgba();
        }
        // GPUI paints `RenderImage`s; the pixels reach it as PNG, the one image format the toolkit
        // takes without an image-crate dependency (`Image::from_bytes` + `use_render_image`).
        let raster = ExportRaster::new(shared(target));
        let bytes = ImageExporter::png_data(&raster).ok()?;
        let image = Arc::new(Image::from_bytes(ImageFormat::Png, bytes));
        let render_image = image.use_render_image(window, cx)?;
        let mut cache = cache.borrow_mut();
        cache.key = Some(key);
        cache.image = Some(render_image.clone());
        Some(render_image)
    }

    /// `handleKeyboardZoom(_:)`: ⌘= / ⌘- / ⌘0 / ⌘1 from the canvas's own key events.
    fn handle_keyboard_zoom(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !event.keystroke.modifiers.platform {
            return false;
        }
        match event.keystroke.key.as_str() {
            "=" | "+" => {
                self.session.update(cx, |session, _| session.zoom_keyboard(1));
                true
            }
            "-" => {
                self.session.update(cx, |session, _| session.zoom_keyboard(-1));
                true
            }
            "0" => {
                self.session.update(cx, |session, _| session.fit_canvas());
                true
            }
            "1" => {
                self.session.update(cx, |session, _| session.actual_pixels());
                true
            }
            _ => false,
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let point = self.canvas_point(event.position);
        let flags = event.modifiers;
        window.focus(&self.focus, cx);
        let snap = self.session.read_with(cx, |session, _| Snap::read(session, point));
        if !snap.has_document || snap.busy {
            return;
        }
        if snap.picking && !self.space_held {
            self.drag = Some(Drag::Sampling);
            let original = self.session.read_with(cx, |session, _| {
                session
                    .color_picker
                    .as_ref()
                    .map(|picker| picker.color())
                    .unwrap_or_else(|| session.foreground_color())
            });
            self.sampling_original = original;
            self.sampling_point = Some(point);
            self.session
                .update(cx, |session, _| session.sample_into_color_picker(snap.pixel));
            return;
        }
        if self.space_held || snap.tool == NavigationTool::Hand {
            self.drag = Some(Drag::Pan);
            self.last_drag_point = Some(point);
            self.cursor = CursorStyle::ClosedHand;
            return;
        }
        match snap.tool {
            NavigationTool::Brush
            | NavigationTool::SpotHealing
            | NavigationTool::CloneStamp
            | NavigationTool::Blur => {
                if snap.tool == NavigationTool::CloneStamp && flags.alt {
                    self.session
                        .update(cx, |session, _| session.set_clone_source(snap.pixel));
                    return;
                }
                self.brush_pointer = Some(point);
                let shift = flags.shift;
                let pixel = snap.pixel;
                self.session.update(cx, |session, _| session.brush_pointer = Some(point));
                self.session.update(cx, |session, _| {
                    if shift {
                        if let Some(from) = session.shift_line_start() {
                            session.begin_brush(from);
                            session.continue_brush(pixel);
                            return;
                        }
                    }
                    session.begin_brush(pixel);
                });
                self.brush_axis_anchor = flags.shift.then_some(pixel);
                self.brush_axis_horizontal = None;
                self.brush_last_pixel = Some(pixel);
                self.drag = Some(Drag::Brush);
            }
            NavigationTool::Marquee | NavigationTool::Lasso | NavigationTool::Wand => {
                let mode = if flags.shift {
                    SelectionMode::Add
                } else if flags.alt {
                    SelectionMode::Subtract
                } else {
                    snap.mode
                };
                self.marquee_constrain_armed = !flags.shift;
                self.drag = Some(Drag::Selection);
                // In New mode, dragging inside the selection moves its outline instead of drawing
                // (`lassoMouseDown(at:event:)`'s replace-mode branch).
                let polygonal = snap.draft_kind == Some(LassoKind::Polygonal);
                let moving = !polygonal
                    && mode == SelectionMode::Replace
                    && self.session.update(cx, |session, _| {
                        session.can_move_selection(snap.pixel) && session.begin_selection_move()
                    });
                if moving {
                    self.selection_drag_start = Some(snap.pixel);
                } else {
                    self.session
                        .update(cx, |session, _| session.begin_lasso(snap.pixel, mode));
                }
            }
            NavigationTool::Gradient => {
                self.drag = Some(Drag::Gradient(GradientHandle::End));
                self.session
                    .update(cx, |session, _| session.begin_gradient(snap.pixel));
            }
            NavigationTool::Type => {
                self.begin_text_gesture(point, cx);
            }
            NavigationTool::Shape => {
                self.session
                    .update(cx, |session, _| session.begin_shape(Self::snapped_corner(snap.pixel)));
            }
            NavigationTool::Crop => {
                // `beginCropDrag(at:)`: a handle resizes the frame, a press inside it moves it,
                // anywhere else starts a new one (and clears the uncommitted frame).
                let pixel = snap.pixel;
                let (rect, mode) = self.session.update(cx, |session, _| {
                    let rect = session
                        .visible_crop_rect()
                        .unwrap_or(Rect::new(pixel.x, pixel.y, 0.0, 0.0));
                    let size = session
                        .document
                        .as_ref()
                        .map(|document| document.size())
                        .unwrap_or(Size::ZERO);
                    let mode =
                        if let Some((index, _)) = crop_resize_regions(session)
                            .into_iter()
                            .find(|(_, region)| region.contains(point))
                        {
                            CropDragMode::Resize(index)
                        } else if session.crop_rect.is_some_and(|crop| crop.contains(pixel))
                            && rect != Rect::new(0.0, 0.0, size.width, size.height)
                        {
                            CropDragMode::Move
                        } else {
                            session.crop_rect = None;
                            CropDragMode::Create
                        };
                    (rect, mode)
                });
                let tolerance = CROP_SNAP_DISTANCE / snap.points_per_pixel.max(0.0001);
                let (xs, ys) = snap.crop_snap_targets.clone();
                self.crop_snap = Some(CropSnap::new(xs, ys, tolerance));
                self.drag = Some(Drag::Crop(CropDrag {
                    start: pixel,
                    original: rect,
                    mode,
                }));
                self.session.update(cx, |session, _| session.start_crop_tool());
            }
            NavigationTool::Move => {
                if snap.shows_guides && !snap.locks_guides {
                    if let Some(guide) = snap.guide_hit {
                        self.drag = Some(Drag::Guide);
                        self.session
                            .update(cx, |session, _| session.begin_guide_move(guide));
                        return;
                    }
                }
                let Some(original) = snap.transform_original else {
                    return;
                };
                let persistent = snap.transform_persistent;
                let mode = if persistent { TransformDragMode::Move } else { TransformDragMode::Move };
                self.drag = Some(Drag::Transform(TransformDrag {
                    original,
                    start: snap.pixel,
                    mode,
                    original_corners: None,
                }));
                if !persistent {
                    self.session
                        .update(cx, |session, _| session.begin_transform(false));
                }
            }
            NavigationTool::Zoom => {
                self.drag = Some(Drag::Zoom {
                    start: point,
                    zoom: snap.zoom,
                    moved: false,
                });
            }
            _ => {}
        }
    }

    fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The right and middle buttons' drags: gpui reports every move with the pressed button,
        // where AppKit sent them to `rightMouseDragged`/`otherMouseDragged`.
        match event.pressed_button {
            Some(MouseButton::Right) if self.brush_tip_drag.is_some() => {
                self.right_mouse_move(event, window, cx);
                return;
            }
            Some(MouseButton::Middle) if self.middle_pan_point.is_some() => {
                self.middle_mouse_move(event, cx);
                return;
            }
            _ => {}
        }
        let point = self.canvas_point(event.position);
        let flags = event.modifiers;
        let snap = self.session.read_with(cx, |session, _| Snap::read(session, point));
        if !snap.has_document {
            return;
        }
        // `if textBoxAnchor != nil { dragTextGesture(to: point); return }`.
        if self.text_box_anchor.is_some() {
            self.drag_text_gesture(point, cx);
            return;
        }
        match self.drag.clone() {
            Some(Drag::Pan) => {
                if let Some(last) = self.last_drag_point {
                    let delta = Size::new(point.x - last.x, point.y - last.y);
                    self.session.update(cx, |session, _| session.pan(delta));
                    self.last_drag_point = Some(point);
                }
                return;
            }
            Some(Drag::Zoom { start, zoom, moved }) => {
                let dx = point.x - start.x;
                let moved = moved || dx.abs() >= 3.0;
                self.drag = Some(Drag::Zoom { start, zoom, moved });
                if moved {
                    let value = zoom * 2.0_f64.powf(dx / 100.0);
                    self.session
                        .update(cx, |session, _| session.zoom(value, Some(start)));
                }
                return;
            }
            Some(Drag::Sampling) => {
                self.sampling_point = Some(point);
                self.session
                    .update(cx, |session, _| session.sample_into_color_picker(snap.pixel));
                return;
            }
            Some(Drag::Selection) => {
                // `dragSelection(to:flags:)`: the grabbed pixel follows the pointer, Shift keeps
                // the move on one axis, Control skips the snapping.
                if let Some(start) = self.selection_drag_start {
                    let pixel = snap.pixel;
                    let mut offset = Size::new(pixel.x - start.x, pixel.y - start.y);
                    let mut horizontal = true;
                    let mut vertical = true;
                    if flags.shift {
                        if offset.width.abs() >= offset.height.abs() {
                            offset.height = 0.0;
                            vertical = false;
                        } else {
                            offset.width = 0.0;
                            horizontal = false;
                        }
                    }
                    if flags.control {
                        self.session
                            .update(cx, |session, _| session.snap_guides = (Vec::new(), Vec::new()));
                    } else {
                        let tolerance =
                            TransformSnap::DISTANCE / snap.points_per_pixel.max(0.0001);
                        offset = self.session.update(cx, |session, _| {
                            session.snapped_selection_offset(offset, tolerance, horizontal, vertical)
                        });
                    }
                    self.session
                        .update(cx, |session, _| session.move_selection(offset));
                    return;
                }
                let square = flags.shift && self.marquee_constrain_armed;
                let pixel = snap.pixel;
                let kind = snap.draft_kind;
                self.session.update(cx, |session, _| match kind {
                    Some(LassoKind::Freehand) => session.extend_lasso(pixel),
                    Some(LassoKind::Polygonal) => session.move_lasso_cursor(Some(pixel)),
                    _ => session.drag_marquee(pixel, square, false),
                });
                return;
            }
            Some(Drag::Guide) => {
                let pixel = snap.pixel;
                if let Some(axis) = snap.guide_axis {
                    let position = match axis {
                        CanvasGuideAxis::Vertical => pixel.x,
                        CanvasGuideAxis::Horizontal => pixel.y,
                    };
                    self.session
                        .update(cx, |session, _| session.move_guide_drag(position));
                }
                return;
            }
            Some(Drag::Crop(drag)) => {
                let pixel = snap.pixel;
                let ratio = snap.crop_ratio;
                let updated = drag.updated(pixel, ratio, flags.alt);
                self.session
                    .update(cx, |session, _| session.crop_rect = Some(updated));
                return;
            }
            Some(Drag::Transform(drag)) => {
                if self.duplicates_transform_on_drag {
                    self.duplicates_transform_on_drag = false;
                    self.session
                        .update(cx, |session, _| session.begin_duplicate_transform());
                }
                let pixel = snap.pixel;
                let lock_ratio = snap.locks_transform_ratio;
                let shift = flags.shift;
                let option = flags.alt;
                if let Some(corners) = drag.corners(pixel, shift) {
                    self.session
                        .update(cx, |session, _| session.preview_corners(&corners));
                    return;
                }
                let updated = drag.updated(pixel, lock_ratio, shift, option).rounded();
                self.session
                    .update(cx, |session, _| session.preview_transform(updated));
                return;
            }
            Some(Drag::Gradient(handle)) => {
                let mut pixel = snap.pixel;
                if flags.shift {
                    if let (Some(start), Some(end)) = (snap.gradient_start, snap.gradient_end) {
                        let around = match handle {
                            GradientHandle::Start => end,
                            GradientHandle::End => start,
                        };
                        pixel = Self::snapped(pixel, around);
                    }
                }
                let start = (handle == GradientHandle::Start).then_some(pixel);
                let end = (handle == GradientHandle::End).then_some(pixel);
                self.session
                    .update(cx, |session, _| session.move_gradient(start, end));
                return;
            }
            Some(Drag::Brush) => {
                self.set_brush_pointer(Some(point), cx);
                let mut pixel = snap.pixel;
                // Shift keeps the stroke straight, horizontal or vertical, from wherever it was
                // pressed; the axis is settled by the first few pixels of movement.
                if flags.shift {
                    let anchor = self.brush_axis_anchor.or(self.brush_last_pixel).unwrap_or(pixel);
                    if self.brush_axis_anchor.is_none() {
                        self.brush_axis_anchor = Some(anchor);
                        self.brush_axis_horizontal = None;
                    }
                    if self.brush_axis_horizontal.is_none()
                        && ((pixel.x - anchor.x).powi(2) + (pixel.y - anchor.y).powi(2)).sqrt() >= 3.0
                    {
                        self.brush_axis_horizontal = Some((pixel.x - anchor.x).abs() >= (pixel.y - anchor.y).abs());
                    }
                    pixel = match self.brush_axis_horizontal {
                        Some(true) => Point::new(pixel.x, anchor.y),
                        Some(false) => Point::new(anchor.x, pixel.y),
                        None => anchor,
                    };
                } else {
                    self.brush_axis_anchor = None;
                    self.brush_axis_horizontal = None;
                }
                self.brush_last_pixel = Some(pixel);
                self.session
                    .update(cx, |session, _| session.continue_brush(pixel));
                return;
            }
            None => {}
        }

        // A guide pulled off a ruler: AppKit delivered the drag to the ruler view wherever the
        // pointer went; gpui delivers it to the element under the pointer, so the canvas carries
        // the drag on here.
        if let Some(axis) = snap.guide_axis {
            let position = match axis {
                CanvasGuideAxis::Vertical => snap.pixel.x,
                CanvasGuideAxis::Horizontal => snap.pixel.y,
            };
            self.session.update(cx, |session, _| session.move_guide_drag(position));
            return;
        }

        // No drag: the pointer moves the brush cursor and picks the cursor style.
        if snap.tool.is_brush_tool() {
            self.set_brush_pointer(Some(point), cx);
        }
        self.cursor = Self::tool_cursor(self.session.read(cx), self.space_held, flags.alt);
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The other buttons' releases go to their own handlers (`rightMouseUp`, `otherMouseUp`).
        match event.button {
            MouseButton::Right => {
                self.right_mouse_up(event, cx);
                return;
            }
            MouseButton::Middle => {
                self.middle_mouse_up(event, window, cx);
                return;
            }
            _ => {}
        }
        let point = self.canvas_point(event.position);
        let flags = event.modifiers;
        let snap = self.session.read_with(cx, |session, _| Snap::read(session, point));
        let drag = self.drag.take();
        // The Swift checks these before its drag state machine: a text box being dragged out, and
        // a guide started on a ruler (gpui reports the release to whatever element is under the
        // pointer, where AppKit sent it back to the view that began the drag).
        if self.text_box_anchor.is_some() {
            self.finish_text_gesture(cx);
            return;
        }
        if drag.is_none() {
            let over_ruler = self
                .session
                .read_with(cx, |session, _| session.is_over_ruler(point));
            self.session
                .update(cx, |session, _| session.finish_guide_drag(over_ruler));
        }
        match drag {
            Some(Drag::Zoom { start, moved, .. }) => {
                if !moved {
                    let value = snap.zoom * if flags.alt { 0.5 } else { 2.0 };
                    self.session
                        .update(cx, |session, _| session.zoom(value, Some(start)));
                }
            }
            Some(Drag::Guide) => {
                let over_ruler = self.session.read_with(cx, |session, _| session.is_over_ruler(point));
                self.session
                    .update(cx, |session, _| session.finish_guide_drag(over_ruler));
            }
            Some(Drag::Brush) => {
                let pixel = snap.pixel;
                self.session.update(cx, |session, _| {
                    session.continue_brush(pixel);
                    session.finish_brush_immediately();
                });
            }
            Some(Drag::Gradient(_)) => {
                self.session.update(cx, |session, _| session.end_gradient_drag());
            }
            Some(Drag::Selection) => {
                self.selection_drag_start = None;
                let pixel = snap.pixel;
                let moved = snap.selection_move_origin;
                let tool = snap.tool;
                let wand_object = snap.wand_object;
                self.session.update(cx, |session, _| {
                    if moved {
                        session.end_selection_move();
                    } else if tool == NavigationTool::Wand && wand_object {
                        session.select_object(pixel, SelectionMode::Replace);
                    } else if tool == NavigationTool::Wand {
                        session.magic_wand(pixel, SelectionMode::Replace);
                    } else {
                        session.finish_lasso();
                    }
                });
            }
            Some(Drag::Transform(_)) => {
                self.duplicates_transform_on_drag = false;
                let persistent = snap.transform_persistent;
                self.session.update(cx, |session, _| {
                    if !persistent {
                        session.commit_transform();
                    }
                });
            }
            Some(Drag::Crop(_)) => {
                self.crop_snap = None;
            }
            Some(Drag::Pan) | None => {}
            Some(Drag::Sampling) => {
                // `if samplingColor { samplingColor = false; … }`.
                self.sampling_point = None;
            }
        }
        self.last_drag_point = None;
        self.cursor = Self::tool_cursor(self.session.read(cx), self.space_held, flags.alt);
    }

    /// The right button resizes the brush tip without painting (`rightMouseDown/Dragged/Up`).
    fn right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let point = self.canvas_point(event.position);
        let snap = self.session.read_with(cx, |session, _| Snap::read(session, point));
        if !snap.tool.is_brush_tool() || self.space_held {
            return;
        }
        let (diameter, hardness) = self
            .session
            .read_with(cx, |session, _| (session.brush_settings.diameter, session.brush_settings.hardness));
        self.brush_tip_drag = Some(BrushTipDrag {
            start: point,
            diameter,
            hardness,
            hardness_shown: event.modifiers.shift,
        });
        self.set_brush_pointer(Some(point), cx);
    }

    fn right_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.brush_tip_drag else {
            return;
        };
        let point = self.canvas_point(event.position);
        let dx = point.x - drag.start.x;
        let shift = event.modifiers.shift;
        self.brush_tip_drag = Some(BrushTipDrag {
            hardness_shown: shift,
            ..drag
        });
        let points_per_pixel = self
            .session
            .read_with(cx, |session, _| session.viewport.points_per_pixel());
        self.session.update(cx, |session, _| {
            if shift {
                // The full hardness range across 200 points.
                session.brush_settings.hardness = (drag.hardness + dx / 200.0).clamp(0.0, 1.0);
                session.brush_settings.diameter = drag.diameter;
            } else {
                // The circle's edge follows the pointer: each point moved widens the radius by a
                // point on screen.
                let per_pixel = points_per_pixel.max(0.0001);
                session.brush_settings.diameter = (drag.diameter + 2.0 * dx / per_pixel).round().clamp(1.0, 2000.0);
                session.brush_settings.hardness = drag.hardness;
            }
        });
        self.brush_pointer = Some(drag.start);
        self.session.update(cx, |session, _| session.brush_pointer = Some(drag.start));
    }

    fn right_mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.brush_tip_drag.take().is_some() {
            self.set_brush_pointer(Some(self.canvas_point(event.position)), cx);
        }
    }

    /// The middle button pans from any tool (`otherMouseDown/Dragged/Up`).
    fn middle_mouse_down(&mut self, event: &MouseDownEvent) {
        self.middle_pan_point = Some(self.canvas_point(event.position));
        self.cursor = CursorStyle::ClosedHand;
    }

    fn middle_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(last) = self.middle_pan_point else {
            return;
        };
        let point = self.canvas_point(event.position);
        let delta = Size::new(point.x - last.x, point.y - last.y);
        self.session.update(cx, |session, _| session.pan(delta));
        self.middle_pan_point = Some(point);
    }

    fn middle_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.middle_pan_point.take().is_some() {
            self.cursor = Self::tool_cursor(self.session.read(cx), self.space_held, false);
        }
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let point = self.canvas_point(event.position);
        let snap = self.session.read_with(cx, |session, _| Snap::read(session, point));
        if !snap.has_document || self.drag.is_some() {
            return;
        }
        let (dx, dy) = match event.delta {
            ScrollDelta::Pixels(delta) => (
                f64::from(f32::from(delta.x)),
                f64::from(f32::from(delta.y)),
            ),
            ScrollDelta::Lines(delta) => (f64::from(delta.x) * 12.0, f64::from(delta.y) * 12.0),
        };
        if event.modifiers.platform || event.modifiers.alt {
            let value = snap.zoom * (-dy * 0.015).exp();
            self.session
                .update(cx, |session, _| session.zoom(value, Some(point)));
        } else {
            self.session
                .update(cx, |session, _| session.pan(Size::new(dx, dy)));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.handle_keyboard_zoom(event, cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "space" => {
                self.space_held = true;
                self.cursor = CursorStyle::OpenHand;
            }
            "escape" => {
                self.text_box_anchor = None;
                self.session.update(cx, |session, _| {
                    session.cancel_text();
                    session.cancel_brush();
                    session.cancel_gradient();
                    session.cancel_shape();
                    session.cancel_crop();
                    session.cancel_transform();
                });
            }
            "enter" => {
                self.session.update(cx, |session, _| {
                    if session.text_draft.is_some() {
                        session.finish_text();
                    } else if session.gradient_edit.is_some() {
                        session.commit_gradient();
                    } else if session.crop_rect.is_some() {
                        session.commit_crop();
                    }
                });
            }
            "delete" | "backspace" => {
                self.session.update(cx, |session, _| session.delete_key_pressed());
            }
            "arrowleft" | "arrowright" | "arrowup" | "arrowdown" => {
                let (dx, dy) = match key {
                    "arrowleft" => (-1.0, 0.0),
                    "arrowright" => (1.0, 0.0),
                    "arrowup" => (0.0, -1.0),
                    _ => (0.0, 1.0),
                };
                let step = if event.keystroke.modifiers.shift { 10.0 } else { 1.0 };
                self.session
                    .update(cx, |session, _| session.nudge_layer(dx * step, dy * step));
            }
            _ => {}
        }
    }

    fn key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "space" {
            self.space_held = false;
            self.cursor = Self::tool_cursor(self.session.read(cx), false, false);
        }
    }

    /// A drop of files onto the canvas imports them where they were dropped (`onDrop`'s
    /// `ImageFileDrop.importProviders`).
    fn drop_paths(&mut self, paths: &ExternalPaths, position: WindowPoint<Pixels>, cx: &mut Context<Self>) {
        let point = self.canvas_point(position);
        let host = self.host.clone();
        let urls: Vec<PathBuf> = paths.paths().to_vec();
        self.session.update(cx, |session, _| {
            let at = session
                .document
                .as_ref()
                .map(|document| session.viewport.document_point(point, document.size()));
            session.import_images(&urls, at, host.as_ref());
        });
    }
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A focus request from the session (`canvasFocusRequest`) takes the keyboard, as the AppKit
        // view did when SwiftUI bumped the counter.
        let request = self.session.read(cx).canvas_focus_request;
        if request != self.last_focus_request {
            self.last_focus_request = request;
            window.focus(&self.focus, cx);
        }
        let cursor = self.cursor;
        let origin = self.origin.clone();
        let cache = self.cache.clone();
        let session = self.session.clone();
        let scale = f64::from(window.scale_factor());
        // `brushTipDrag?.hardnessShown == true ? session.brushSettings.hardness : nil`.
        let hardness = self
            .brush_tip_drag
            .filter(|drag| drag.hardness_shown)
            .map(|_| self.session.read(cx).brush_settings.hardness);
        let text_box_rect = self.text_box_rect;
        let sample_ring = {
            let session = self.session.read(cx);
            if session.shows_sample_ring {
                self.sampling_point.map(|point| SampleRingState {
                    point,
                    original: self.sampling_original,
                    sampled: session
                        .color_picker
                        .as_ref()
                        .map(|picker| picker.color())
                        .unwrap_or_else(|| session.foreground_color()),
                })
            } else {
                None
            }
        };

        div()
            .id("editor-canvas")
            .relative()
            .size_full()
            .overflow_hidden()
            .cursor(cursor)
            .track_focus(&self.focus)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        origin.set(bounds.origin);
                        let size = Size::new(
                            f64::from(f32::from(bounds.size.width)),
                            f64::from(f32::from(bounds.size.height)),
                        );
                        let document_size = session.read(cx).document.as_ref().map(|d| d.size());
                        if session.read(cx).viewport.view_size != size {
                            session.update(cx, |session, cx| {
                                session.viewport.resize(size, scale, document_size);
                                cx.notify();
                            });
                        }
                        CanvasView::raster(&session, size, &cache, window, cx)
                    },
                    move |bounds, image: Option<Arc<RenderImage>>, window, _cx| {
                        if let Some(image) = image {
                            let _ = window.paint_image(bounds, bounds, Corners::default(), image, 0, false);
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .children(overlay_stack(&self.session, hardness, text_box_rect, sample_ring))
            .on_hover(cx.listener(Self::hover_changed))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::right_mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::middle_mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                let position = window.mouse_position();
                this.drop_paths(paths, position, cx);
            }))
            .can_drop(|any, _, _| any.is::<ExternalPaths>())
    }
}

/// The overlays stacked over the composite, back to front: the lines (guides and marching ants),
/// the transform box, the brush cursor and the sample ring.
fn overlay_stack(
    session: &Entity<EditorSession>,
    hardness: Option<f64>,
    text_box_rect: Option<Rect>,
    sample_ring_state: Option<SampleRingState>,
) -> Vec<AnyElement> {
    vec![
        canvas_lines(session.clone(), text_box_rect).into_any_element(),
        transform_overlay(session.clone()).into_any_element(),
        brush_cursor(session.clone(), hardness).into_any_element(),
        sample_ring_state
            .map(|state| sample_ring(state).into_any_element())
            .unwrap_or_else(|| div().into_any_element()),
    ]
}
