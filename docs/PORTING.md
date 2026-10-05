# Porting Compositor (Swift/macOS) to Rust (cross-platform)

Upstream: `references/Compositor` @ `11d8d7a50992b24fd9a760a1c13b1c01b70aaf30` (Compositor 1.4.5, MIT).
This document is the contract every porting slice follows. **Read it before touching anything.**

The port is *behavior-preserving*: same features, same numbers, same limits, same UI structure and wording.
Nothing is dropped, simplified or "cleaned up". Where an Apple API has no cross-platform equivalent, the
substitution is listed below and must reproduce the same observable behavior.

## 1. Crate map

| Crate | Responsibility | Swift origin |
|---|---|---|
| `compositor-core` | Geometry, color, pixel buffers, blend-mode enum, document model (layers, masks, adjustments, effects, shapes, text, guides, groups, palette, limits), `DocumentHistory`, selection geometry, transform/snap math, tool settings. No UI, no IO, no GPU. | `Document/**` (model parts), `Rendering/CanvasViewport.swift`, `UI/SliderSnap.swift` |
| `compositor-pixels` | CPU pixel kernels: 1:1 ports of `Rendering/*.c` plus the Swift pixel algorithms (blur, noise, dither, heal, content fill, wand, lens, levels, camera raw, layer effects rasterization, brush rasterization, downsample). | `Rendering/*.c|*.h`, pixel paths of `Document/**` |
| `compositor-render` | Compositing pipeline: layer renderer, tiled renderer, adjustment surface, effects surface, live mask renderer, raster snapshot, preview caches, warping. | `Rendering/**` |
| `compositor-session` | `EditorSession`: every piece of editor state and every command (tools, panels' state, drafts, undo transactions, clipboard, crop, gradient, type, brush, warp…). No UI toolkit types. | `Document/EditorSession*.swift`, tool/menu command bodies from `UI/**` |
| `compositor-io` | `.comp` project store, PSD read/write, image import/export, camera RAW, recent projects, file watcher, digest, `ProjectController`. | `IO/**` |
| `compositor-ui` | gpui views: window chrome, tool rail, tool headers, layers panel, canvas, sheets, floating panels, tabs, shortcuts, menus. | `UI/**`, `ContentView.swift`, view parts of `CompositorApp.swift` |
| `compositor-app` | Binary: application delegate, project workspace, per-window wiring, drag & drop, window management. | `CompositorApp.swift`, `IO/CompositorApplicationDelegate.swift`, `UI/ProjectWindowBridge.swift`, `UI/ProjectTabLayout.swift` |

Dependency direction (no cycles): `core ← pixels ← render ← session ← io ← ui ← app`.

## 2. Naming

* Types keep their Swift names exactly (`CanvasDocument`, `ImageLayer`, `LayerMask`, `TransformEdit`, …).
  Trailing `View` types in `compositor-ui` keep it too (`LayersPanelView`, `CropControlsView`).
* Enum cases: Swift `case colorBurn` → Rust `ColorBurn`. Raw values (strings used in the manifest) are kept verbatim.
* Methods/fields: Swift `func blendPreview...` / `var isVisible` → Rust `fn`/field `snake_case`.
* Swift argument labels become positional parameters; named parameters disappear:
  `func scale(_ percent: CGFloat, about center: CGPoint)` → `fn scale(&mut self, percent: f64, about: Point)`.
* Keep the original doc comments (`///`) — translate them, do not delete them.
* American spelling ("color", "gray", "center"). Numeric literals keep their exact values and underscore grouping.
* Constants: `DocumentLimits::MAX_SIDE`, `MAX_SURFACE_PIXELS`, `DOCUMENT_PIXEL_BUDGET` (fn, it reads host memory).

## 3. Numeric and pixel conventions

* All document geometry is `f64` (Swift `CGFloat` on 64-bit). Pixel/color math inside kernels is `f32` where the C
  used `float`, `f64` where the C used `double` — **match the C signatures exactly**.
* Canonical pixels: **premultiplied sRGB RGBA8, 4 bytes/pixel, rows top-down, no padding** (`compositor_core::Rgba8Image`).
  Masks/selections: **8-bit gray, 1 byte/pixel** (`Gray8Image`); white reveals / selected, black hides.
* Coordinates are document pixels; `Rect` origin is top-left, y **down** (the Swift code already works in top-left
  document space; do not flip).
* `LayerTransform.rotation` is clockwise degrees.
* Kernels take `stride` bytes-per-row like the C did; the Rust wrappers pass `width * 4` / `width`.

## 4. Platform substitutions

| Apple | Rust | Notes |
|---|---|---|
| `CGFloat`/`CGPoint`/`CGSize`/`CGRect` | `core::geom::{Point, Size, Rect}` | `CGRect.intersection` → `Rect::intersection` (returns `Option`-free `Rect` with `is_null`/`is_empty`, mirroring CGRect), `.integral`, `.insetBy`, `.offsetBy`, `.union`, `.standardized`, `contains`, `intersects` |
| `CGAffineTransform` | `core::geom::AffineTransform` | a,b,c,d,tx,ty; `concatenating`, `inverted`, `applying` |
| `CGImage` | `core::Rgba8Image` (immutable, `Arc`-shared) + `render::RasterSnapshot` | no bitmap handles |
| `CGContext` (bitmap) | `core::Rgba8Image` / `Gray8Image` as drawing target | drawing ops live in `compositor-pixels::raster` |
| Core Image filters (`CIFilter`, `SeparableBlend`) | hand-written blend kernels in `compositor-pixels::blend` | must match Photoshop/Core Image formulas; sRGB (non-linear) space |
| Metal (`MetalLayerEffects`, `MetalWarp`, `MetalBrushCoverage`, `GPUNoise`, `GPUCanvas`) | CPU kernels in `compositor-pixels`, parallelized with `rayon` | same visuals; the renderer keeps a trait seam so a wgpu backend can be added later |
| Core Graphics drawing (paths, gradients, text) | `compositor-pixels::raster` (scanline AA fill, gradients) + `compositor-pixels::text` (glyph raster) | antialiasing rules preserved |
| Core Text (`CTFont`, `NSFont`) | `cosmic-text`/`fontdue` glyph raster; font name strings kept verbatim | text layer metadata keeps PostScript font names |
| ImageIO (`CGImageSource/Destination`) | `image` crate + `png` + `jpeg` encode/decode | sRGB, 8-bit, premultiplied conversions preserved |
| `NSImage` SVG decode | `resvg` rasterization into `Rgba8Image` | SVG import becomes pixels, as upstream |
| Camera RAW (`CIRAWFilter`) | `rawler` decode + `compositor-pixels::camera_raw` for the develop controls | develop-sheet parameters are identical |
| AppKit drag & drop, panels, menus, sheets, tables, text fields | `gpui` + `gpui-kit` widgets; floating panels become child windows/overlays | structure and labels preserved |
| `@AppStorage` / `UserDefaults` / `ToolDefaults` | `core::settings::ToolDefaults` backed by a JSON file in the config dir | same keys |
| `NSFileCoordinator`, FSEvents | `notify` + atomic temp-dir-then-rename (package replacement) | same guarantees |
| Sparkle auto-update | `compositor-app::updates` — appcast feed is still published/parsed (`appcast.xml`), the installer is platform-specific | feature preserved as feed check + release link |
| `Task`/`async` UI coordination (`CheckedContinuation` waiters, busy flag) | synchronous commands + a `Busy` flag on `EditorSession`; background work through `rayon`/`std::thread` with a completion queue drained on the UI thread | observable states (`showsBusy`, `isProjectBusy`, `canUndo`, …) preserved |

## 5. Persistence

`docs/project-format.md` is normative. `compositor-io::project_store` reads versions 1–11 and writes 11, with the
exact validation rules (limits, cycles, dangling parents, unsafe paths, oversized assets) and the exact JSON keys
(they are the Swift `Codable` keys: `id`, `imageFile`, `maskFile`, `blendMode`, `parentID`, …).

## 6. UI rules

* Every menu, sheet, panel, tool, keyboard shortcut and status-bar item in `CompositorApp.swift`,
  `KeyboardShortcuts.swift`, `ContentView.swift` and `UI/**` must exist, with the same label text (macOS glyphs
  `⌘⇧⌥⌃` become `Ctrl/Shift/Alt/Cmd` modifiers; the *default* bindings and remapping stay identical).
* Tool rail order, tool labels and tooltips, tool-header controls per tool, panel layouts and the Layers panel
  interactions (drag to reorder, Option-drag duplicate, right-click menu, inline rename) are preserved.
* Keyboard/menu semantics (Undo/Redo rules, Escape cancels, Return commits, Tab cycles tool modes, `[`/`]` brush
  size, modifiers for selection add/subtract, ⌘-drag distort, Shift constraints) are preserved exactly.

## 7. Verification

* Pure-logic ports are covered by ports of `CompositorTests` (14k lines) — port the arithmetic assertions, not the
  XCTest scaffolding. Every ported test keeps its original name (`adjustment_layer_tests` → `#[test] fn …`).
* `cargo test --workspace` must pass; the app must build and run on Windows (`cargo run -p compositor-app`), with
  a fresh screenshot as proof for UI work.
* Do not add tests for wiring/forwarding; do not delete a ported assertion because it is inconvenient.
