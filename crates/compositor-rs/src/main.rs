//! The Compositor application: one window, its menu bar, the commands the app shell owns and the
//! files handed to the process.
//!
//! Ported from `CompositorApp.swift`, `IO/CompositorApplicationDelegate.swift` and
//! `UI/ProjectWindowBridge.swift`: a window titled `Compositor` showing the project workspace, the
//! application commands, the launch and second-launch file hand-off, and the quit/close flows.
//!
//! The window root is [`AppRoot`]: it draws the component title bar with the menu bar in it, the
//! workspace view, and the app-level floating panels and sheets (`Keyboard Shortcuts…`, the JPEG
//! export card, the Photoshop conversion and Camera Raw develop cards) that no tab view owns. On
//! Windows there is no menu bar for gpui to fill — `App::set_menus` is only stored — so the same
//! menus go to `GlobalState`, which `AppMenuBar` renders.
//!
//! Every gpui dialog is asynchronous, while `compositor_rs_io::project_controller::ProjectPrompter` is
//! synchronous: [`prompt`] holds the replay loop that reconciles the two.

mod commands;
mod host;
mod menu;
mod prompt;
mod updates;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use compositor_rs_core::Id;
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_io::image_exporter::ExportRaster;
use compositor_rs_io::recent_projects::RecentProjects;
use compositor_rs_session::EditorSession;
use compositor_rs_session::projects::{PSDConversionRequest, RawDevelop, SessionHost};
use compositor_rs_ui::ProjectWorkspaceView;
use compositor_rs_ui::actions;
use compositor_rs_ui::content_view::{MIN_HEIGHT, MIN_WIDTH};
use compositor_rs_ui::panels::color_picker::ColorPickerPanelController;
use compositor_rs_ui::panels::floating_panel::{FloatingPanelController, FloatingPanelPlacement};
use compositor_rs_ui::sheets::canvas_size::CanvasSizeSheet;
use compositor_rs_ui::sheets::grid_settings::GridSettingsSheet;
use compositor_rs_ui::sheets::image_size::ImageSizeSheet;
use compositor_rs_ui::sheets::jpeg_export::JpegExportSheet;
use compositor_rs_ui::sheets::keyboard_shortcuts::KeyboardShortcutsSheet;
use compositor_rs_ui::sheets::psd_conversion::PSDConversionSheet;
use compositor_rs_ui::sheets::raw_develop::RawDevelopSheet;
use compositor_rs_ui::sheets::trim::TrimSheet;
use compositor_rs_ui::workspace::ProjectWorkspace;

use gpui_kit::base::GlobalState;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::{Theme, ThemeMode, TitleBar, WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::menu::MenuState;

/// The watcher tick (`ProjectWorkspace.pollTabs`): the controllers' external-change watch, and any
/// paths a second launch queued.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Paths a second launch handed the process, waiting for the next tick.
///
/// On Windows a second launch is another process, so nothing fills this; on macOS the platform
/// delivers Open events through `Application::on_open_urls`, whose callback runs without an `App`
/// handle to reach the workspace with.
static QUEUED_PATHS: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn main() {
    env_logger::init();
    // The whole icon catalog, not the generated default subset: the editor draws Lucide icons the
    // subset leaves out (`lasso`, `crop`, `layers`, `paintbrush`, …) and a missing one renders as
    // nothing — 68 `could not find asset` errors per run with `Assets`.
    let application = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    // `application(_:open:)`: files handed to the running app (a second launch, a drop on the icon).
    application.on_open_urls(|urls| {
        let mut queued = QUEUED_PATHS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        queued.extend(urls.into_iter().map(PathBuf::from));
    });
    application.run(|cx: &mut App| {
        gpui_kit::init(cx);
        // `applicationWillFinishLaunching`: always dark, alerts and open/save panels included.
        Theme::change(ThemeMode::Dark, None, cx);
        // Every remappable shortcut, bound to its action with the override in force.
        cx.bind_keys(compositor_rs_ui::shortcuts::bindings());

        let host: Arc<dyn SessionHost> = Arc::new(host::AppHost::new());
        let workspace = cx.new(|cx| ProjectWorkspace::new(host.clone(), cx));
        let (_handle, root) = gpui_kit::open_window(window_options(), cx, {
            let workspace = workspace.clone();
            let host = host.clone();
            move |window, cx| {
                let root = cx.new(|cx| AppRoot::new(workspace, host, window, cx));
                // Files handed to the process at launch (Windows: argv paths).
                let launch = launch_paths();
                if !launch.is_empty() {
                    root.update(cx, |root, cx| root.receive_paths(launch, window, cx));
                }
                root
            }
        })
        .expect("the Compositor window opens");
        cx.on_window_closed(|cx, _| {
            // The Swift app quit when its last window closed; gpui leaves the run loop to us.
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

/// The window: `Window("Compositor", id: "editor").defaultSize(width: 1180, height: 780)`, with the
/// content view's minimum (`frame(minWidth: 800, minHeight: 520)`), and the component title bar so
/// the menu bar has somewhere to draw on Windows.
fn window_options() -> WindowOptions {
    let mut options = TitleBar::window_options();
    options.window_bounds = Some(WindowBounds::Windowed(Bounds::new(
        point(px(0.0), px(0.0)),
        size(px(1180.0), px(780.0)),
    )));
    options.window_min_size = Some(size(px(MIN_WIDTH), px(MIN_HEIGHT)));
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some("Compositor".into());
    }
    options
}

/// The files the process was handed at launch, after the executable (`application(_:open:)`'s launch
/// half, which SwiftUI did before any window existed).
fn launch_paths() -> Vec<PathBuf> {
    std::env::args_os().skip(1).map(PathBuf::from).collect()
}

/// The window's content: the title bar with the menu bar, the workspace, and the app-level sheets.
struct AppRoot {
    workspace: Entity<ProjectWorkspace>,
    host: Arc<dyn SessionHost>,
    view: Entity<ProjectWorkspaceView>,
    menu_bar: Entity<AppMenuBar>,
    /// The menus as they were last built, so the bar is only replaced when something it shows changed.
    menu_state: Option<MenuState>,
    /// One observer per tab session, so a session change rebuilds the menu bar.
    observed: HashMap<Id, Subscription>,
    /// `ShortcutSettings.show()`'s panel.
    keyboard_shortcuts: FloatingPanelController,
    /// The app's Color Picker, while the picker has to float above a dialog. The editor hosts its
    /// own panel for the colors that are sampled from the canvas (`ContentView`).
    color_picker: ColorPickerPanelController,
    /// The tab whose import panel is up (`session.showsImporter`), so the render pass that sees the
    /// flag starts the panel exactly once.
    importing: Option<Id>,
    /// `ProjectController.exportJPEG`'s sheet, while its card is up.
    jpeg_export: Option<Entity<JpegExportSheet>>,
    /// `ProjectTabs`' PSD conversion sheet, shown while the session says so.
    psd_conversion: Option<(Id, Entity<PSDConversionSheet>)>,
    /// `ProjectTabs`' Camera Raw develop sheet, shown while the session says so.
    raw_develop: Option<(PathBuf, Entity<RawDevelopSheet>)>,
}

impl AppRoot {
    fn new(
        workspace: Entity<ProjectWorkspace>,
        host: Arc<dyn SessionHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| ProjectWorkspaceView::new(workspace.clone(), cx));
        let menu_bar = AppMenuBar::new(cx);
        let mut root = Self {
            workspace: workspace.clone(),
            host,
            view,
            menu_bar,
            menu_state: None,
            observed: HashMap::new(),
            keyboard_shortcuts: FloatingPanelController::new("keyboardShortcuts"),
            color_picker: ColorPickerPanelController::new(),
            importing: None,
            jpeg_export: None,
            psd_conversion: None,
            raw_develop: None,
        };
        root.sync_menus(cx);
        // `windowShouldClose`: the close is intercepted, the Swift `applicationShouldTerminate` flow
        // runs (every project's `confirmQuit` in `quitOrder`), and only then does the window go.
        let closing = workspace.clone();
        window.on_window_should_close(cx, move |window, cx| {
            prompt::run(
                window,
                cx,
                {
                    let workspace = closing.clone();
                    move |_window, cx, prompter| {
                        workspace.update(cx, |workspace, cx| workspace.confirm_quit(prompter, cx))
                    }
                },
                |quit, window, _cx| {
                    if quit {
                        window.remove_window();
                    }
                },
            );
            false
        });
        // The watcher tick: `ProjectWorkspace.pollTabs` and the second-launch queue.
        let weak = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let alive = cx.update(|window, cx| {
                    let queued = std::mem::take(
                        &mut *QUEUED_PATHS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                    );
                    if !queued.is_empty() {
                        let _ = weak.update(cx, |root, cx| root.receive_paths(queued, window, cx));
                    }
                    prompt::run(
                        window,
                        cx,
                        {
                            let workspace = workspace.clone();
                            move |_window, cx, prompter| {
                                workspace.update(cx, |workspace, cx| workspace.poll_tabs(prompter, cx));
                            }
                        },
                        |_, _, _| {},
                    );
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
        root
    }

    /// Rebuilds the menu bar when anything it shows has changed, and feeds both consumers: the
    /// platform's menu bar (macOS draws it) and `GlobalState` (the `AppMenuBar` widget draws it on
    /// Windows and Linux).
    fn sync_menus(&mut self, cx: &mut Context<Self>) {
        let state = {
            let workspace = self.workspace.read(cx);
            let session = workspace.current().session.read(cx);
            let recent = RecentProjects::shared().lock().urls().to_vec();
            MenuState::read(&session, workspace.is_managing(), &recent)
        };
        if self.menu_state.as_ref() == Some(&state) {
            return;
        }
        let menus = state.menus();
        self.menu_state = Some(state);
        cx.set_menus(menus);
        let menus = cx.get_menus().unwrap_or_default();
        GlobalState::global_mut(cx).set_app_menus(menus);
        self.menu_bar.update(cx, |bar, cx| bar.reload(cx));
    }

    /// Watches every tab's session for a frame, so a change anywhere rebuilds the menu bar.
    fn observe_tabs(&mut self, cx: &mut Context<Self>) {
        let tabs: Vec<(Id, Entity<EditorSession>)> = self
            .workspace
            .read(cx)
            .tabs()
            .iter()
            .map(|tab| (tab.id, tab.session.clone()))
            .collect();
        self.observed.retain(|id, _| tabs.iter().any(|(tab, _)| tab == id));
        for (id, session) in tabs {
            if self.observed.contains_key(&id) {
                continue;
            }
            let subscription = cx.observe(&session, |_: &mut Self, _, cx| cx.notify());
            self.observed.insert(id, subscription);
        }
    }

    /// The front tab's session, when it exists (and has a document, for the file commands).
    fn current_session(&self, cx: &App, document: bool) -> Option<Entity<EditorSession>> {
        let session = self.workspace.read(cx).current().session.clone();
        if document && session.read(cx).document.is_none() {
            return None;
        }
        Some(session)
    }

    /// File > Import Images…: `CompositorApp.swift:70` sets the flag; the render pass below opens the
    /// panel, because the sheet's "Import image" asks for the same panel the same way.
    fn request_import(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.current_session(cx, false) {
            session.update(cx, |session, _| session.shows_importer = true);
        }
    }

    /// `ContentView.fileImporter(isPresented: $session.showsImporter, …)`: while a session asks for
    /// the image panel, open it and hand what it returns to `importImages`.
    fn sync_importer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.importing.is_some() {
            return;
        }
        let Some(session) = self.current_session(cx, false) else { return };
        if !session.read(cx).shows_importer {
            return;
        }
        let id = self.workspace.read(cx).current().id;
        self.importing = Some(id);
        let host = self.host.clone();
        let weak = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let outcome = prompt::import_panel(cx).await;
                let _ = weak.update(cx, |root, cx| {
                    root.importing = None;
                    cx.notify();
                });
                let (paths, error) = match outcome {
                    Ok(paths) => (paths, None),
                    Err(message) => (Vec::new(), Some(message)),
                };
                let _ = cx.update(|_window, cx| {
                    session.update(cx, |session, _| {
                        session.shows_importer = false;
                        match error {
                            Some(message) => session.import_error = Some(message),
                            None if !paths.is_empty() => session.import_images(&paths, None, host.as_ref()),
                            None => {}
                        }
                    });
                });
            })
            .detach();
    }

    /// `application(_:open:)` and the window's file drop: a `.comp` opens as a project, anything
    /// else imports into a tab.
    fn receive_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        prompt::run(
            window,
            cx,
            {
                let workspace = self.workspace.clone();
                move |_window, cx, prompter| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.receive_paths(&paths, None, None, prompter, cx);
                    });
                }
            },
            |_, _, _| {},
        );
    }

    /// File > New Canvas… (`ProjectWorkspace.newCanvas()`: a fresh tab beside the current one).
    fn new_canvas(&mut self, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| workspace.new_canvas(cx));
    }

    /// File > Open Project… (`ProjectWorkspace.open`).
    fn open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        prompt::run(
            window,
            cx,
            {
                let workspace = self.workspace.clone();
                move |_window, cx, prompter| {
                    workspace.update(cx, |workspace, cx| workspace.open(None, prompter, cx))
                }
            },
            |_, _, _| {},
        );
    }

    /// File > Open Recent > a project.
    fn open_recent(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let path = PathBuf::from(path);
        prompt::run(
            window,
            cx,
            {
                let workspace = self.workspace.clone();
                move |_window, cx, prompter| {
                    workspace.update(cx, |workspace, cx| workspace.open(Some(&path), prompter, cx))
                }
            },
            |_, _, _| {},
        );
    }

    /// File > Open Recent > Clear Menu.
    fn clear_recent(&mut self, cx: &mut Context<Self>) {
        RecentProjects::shared().lock().clear();
        cx.notify();
    }

    /// File > Save / Save As… (`ProjectController.save`).
    fn save(&mut self, as_new: bool, window: &mut Window, cx: &mut Context<Self>) {
        prompt::run(
            window,
            cx,
            {
                let workspace = self.workspace.clone();
                move |_window, cx, prompter| {
                    workspace.update(cx, |workspace, cx| {
                        let host = workspace.host().clone();
                        let (controller, session) = workspace.current_controller();
                        session.update(cx, |session, _| {
                            controller.save(session, host.as_ref(), prompter, as_new)
                        })
                    })
                }
            },
            |_, _, _| {},
        );
    }

    /// File > Export PNG… (`ProjectController.exportPNG`).
    fn export_png(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.current_session(cx, true) else { return };
        let host = self.host.clone();
        let suggested = suggested_name(&session.read(cx).project_url, "png");
        window.spawn(cx, async move |cx| {
            let Some(path) = prompt::save_panel(cx, &suggested).await else { return };
            let outcome = cx.update(|_window, cx| {
                session.update(cx, |session, _| session.export_png(&path, host.as_ref()))
            });
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(message)) => prompt::show_error(cx, "Couldn’t export PNG", &message),
                Err(_) => {}
            }
        })
        .detach();
    }

    /// File > Export JPEG… (`ProjectController.exportJPEG`): render, the JPEG sheet, then the Save panel.
    fn export_jpeg(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.current_session(cx, true) else { return };
        let Some(snapshot) = session.read(cx).project_snapshot() else { return };
        let rendered = match self.host.render(&snapshot) {
            Ok(raster) => raster,
            Err(message) => {
                prompt::report_error(window, cx, "Couldn’t export JPEG", &message);
                return;
            }
        };
        // The sheet encodes the composited pixels with the document's resolution
        // (`ExportRaster(image:resolution:)`); `render` handed back the sparse raster the
        // adjustment dialog samples, so materialize it and carry the resolution across.
        let image = match rendered.materialize() {
            PixelImage::Rgba(image) => image,
            PixelImage::Gray(_) => unreachable!("an export render is color, not a mask"),
        };
        let raster = ExportRaster::with_resolution(image, snapshot.manifest.resolution.unwrap_or(72.0));
        let weak = cx.entity().downgrade();
        let sheet = cx.new(|cx| {
            JpegExportSheet::new(
                raster,
                session.clone(),
                Arc::new(move |data, window, cx| {
                    let _ = weak.update(cx, |root, cx| root.finish_jpeg_export(data, window, cx));
                }),
                cx,
            )
        });
        self.jpeg_export = Some(sheet);
        cx.notify();
    }

    /// The JPEG sheet's report: bytes go to the Save panel, or nothing on Cancel.
    fn finish_jpeg_export(&mut self, data: Option<Vec<u8>>, window: &mut Window, cx: &mut Context<Self>) {
        self.jpeg_export = None;
        cx.notify();
        let Some(data) = data else { return };
        let Some(session) = self.current_session(cx, true) else { return };
        let host = self.host.clone();
        let suggested = suggested_name(&session.read(cx).project_url, "jpg");
        window.spawn(cx, async move |cx| {
            let Some(path) = prompt::save_panel(cx, &suggested).await else { return };
            let outcome = cx.update(|_window, cx| {
                session.update(cx, |session, _| session.export_jpeg(&data, &path, host.as_ref()))
            });
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(message)) => prompt::show_error(cx, "Couldn’t export JPEG", &message),
                Err(_) => {}
            }
        })
        .detach();
    }

    /// File > Close Project (`ProjectWorkspace.close`).
    fn close_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        prompt::run(
            window,
            cx,
            {
                let workspace = self.workspace.clone();
                move |_window, cx, prompter| {
                    workspace.update(cx, |workspace, cx| {
                        let id = workspace.selected_id();
                        workspace.close(id, prompter, cx);
                    });
                }
            },
            |_, _, _| {},
        );
    }

    /// App menu > Check for Updates…: the appcast feed, as Sparkle would read it.
    fn check_for_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = cx.http_client();
        window.spawn(cx, async move |cx| {
            match updates::check(client, env!("CARGO_PKG_VERSION")).await {
                Ok(updates::UpdateCheck::UpToDate) => {
                    let (title, message) = updates::up_to_date_alert(env!("CARGO_PKG_VERSION"));
                    prompt::show_error(cx, &title, &message);
                }
                Ok(updates::UpdateCheck::Available(release)) => {
                    if let Some(url) = release.download_url() {
                        // Sparkle's installer is platform-specific: a newer release opens its download.
                        let _ = cx.update(|_window, cx| cx.open_url(url));
                    }
                }
                Err(message) => prompt::show_error(cx, "Couldn’t check for updates", &message),
            }
        })
        .detach();
    }

    /// Edit > Keyboard Shortcuts… (`ShortcutSettings.show`).
    fn show_keyboard_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.keyboard_shortcuts.is_visible() {
            return;
        }
        let sheet = KeyboardShortcutsSheet::open(
            |_window, cx| {
                // A remap takes effect at once.
                cx.bind_keys(compositor_rs_ui::shortcuts::bindings());
            },
            window,
            cx,
        );
        let closing = sheet.clone();
        self.keyboard_shortcuts.set_on_close(move |window, cx| {
            closing.update(cx, |sheet, cx| sheet.cancel(window, cx));
        });
        self.keyboard_shortcuts
            .show("Keyboard Shortcuts", sheet, FloatingPanelPlacement::Automatic, window, cx);
    }

    /// Image > Canvas Size… (`ProjectController.canvasSize`).
    fn canvas_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.current_session(cx, true) else { return };
        let finish = session.clone();
        CanvasSizeSheet::open(
            session,
            move |options, window, cx| {
                let Some(options) = options else { return };
                let outcome = finish.update(cx, |session, _| session.change_canvas_size(&options));
                if let Err(error) = outcome {
                    prompt::report_error(window, cx, "Couldn’t change canvas size", &error.to_string());
                }
            },
            window,
            cx,
        );
    }

    /// Image > Image Size… (`ProjectController.imageSize`).
    fn image_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.current_session(cx, true) else { return };
        let finish = session.clone();
        ImageSizeSheet::open(
            session,
            move |options, window, cx| {
                let Some(options) = options else { return };
                let outcome = finish.update(cx, |session, _| session.change_image_size(&options));
                if let Err(error) = outcome {
                    prompt::report_error(window, cx, "Couldn’t resize the image", &error.to_string());
                }
            },
            window,
            cx,
        );
    }

    /// Image > Trim… (`ProjectController.trim`).
    fn trim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.current_session(cx, true) else { return };
        let finish = session.clone();
        TrimSheet::open(
            move |options, window, cx| {
                let Some(options) = options else { return };
                match finish.update(cx, |session, _| session.trim(&options)) {
                    Ok(_) => {}
                    Err(error) => prompt::report_error(window, cx, "Couldn’t trim image", &error.to_string()),
                }
            },
            window,
            cx,
        );
    }

    /// View > Grid Settings… (`ProjectController.gridSettings`): the grid shows while the sheet is up,
    /// changing as it is edited, and goes back to how it was on Cancel.
    fn grid_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.current_session(cx, false) else { return };
        let (original_grid, original_appearance, was_shown) = {
            let session = session.read(cx);
            (session.layout_grid.clone(), session.grid_appearance.clone(), session.shows_grid)
        };
        session.update(cx, |session, _| session.shows_grid = true);
        let preview = session.clone();
        let finish = session.clone();
        let (grid, appearance) = (original_grid.clone(), original_appearance.clone());
        GridSettingsSheet::open(
            session,
            original_grid,
            original_appearance,
            move |grid, appearance, cx| {
                preview.update(cx, |session, _| {
                    session.layout_grid = grid;
                    session.grid_appearance = appearance;
                });
            },
            move |settings, _window, cx| {
                finish.update(cx, |session, _| {
                    session.shows_grid = was_shown;
                    let (grid, appearance) = settings.unwrap_or((grid, appearance));
                    session.layout_grid = grid;
                    session.grid_appearance = appearance;
                });
            },
            window,
            cx,
        );
    }

    /// The Color Picker's second host: the window's dialog layer, which is the only place that draws
    /// over a dialog. Used for the picker a dialog's own swatch opened, and for any picker while a
    /// dialog is up (`ContentView` hosts the editor's panel for the rest).
    fn sync_color_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.current_session(cx, false);
        let dialog_up = window.has_active_dialog(cx);
        self.color_picker.sync_dialog(session, dialog_up, window, cx);
    }

    /// The PSD conversion and Camera Raw sheets, which `ProjectTabs` shows for the front tab.
    fn sync_sheets(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.current_session(cx, false) else {
            self.psd_conversion = None;
            self.raw_develop = None;
            return;
        };
        let (conversion, develop, shows_conversion, shows_develop) = {
            let session = session.read(cx);
            (
                session.conversion_request.clone(),
                session.raw_develop.clone(),
                session.shows_conversion_sheet,
                session.shows_raw_develop,
            )
        };
        let weak = cx.entity().downgrade();
        match conversion.filter(|_| shows_conversion) {
            Some(request) => {
                if self.psd_conversion.as_ref().is_none_or(|(id, _)| *id != request.id) {
                    let id = request.id;
                    let sheet =
                        Self::conversion_sheet(request, session.clone(), self.host.clone(), weak.clone(), cx);
                    self.psd_conversion = Some((id, sheet));
                }
            }
            None => self.psd_conversion = None,
        }
        match develop.filter(|_| shows_develop) {
            Some(develop) => {
                let url = develop.url.clone();
                if self.raw_develop.as_ref().is_none_or(|(open, _)| *open != url) {
                    let sheet =
                        Self::raw_develop_sheet(develop, session.clone(), self.host.clone(), weak, cx);
                    self.raw_develop = Some((url, sheet));
                }
            }
            None => self.raw_develop = None,
        }
    }

    /// The PSD conversion sheet (`PSDConversionSheet`, shown for `showsConversionSheet`).
    fn conversion_sheet(
        request: PSDConversionRequest,
        session: Entity<EditorSession>,
        host: Arc<dyn SessionHost>,
        weak: WeakEntity<Self>,
        cx: &mut Context<Self>,
    ) -> Entity<PSDConversionSheet> {
        cx.new(|_| {
            PSDConversionSheet::new(
                request,
                Arc::new(move |confirmed, _window, cx| {
                    let host = host.clone();
                    session.update(cx, |session, _| session.finish_conversion(confirmed, host.as_ref()));
                    let _ = weak.update(cx, |root, cx| {
                        root.psd_conversion = None;
                        cx.notify();
                    });
                }),
            )
        })
    }

    /// The Camera Raw develop sheet (`RawDevelopSheet`, shown for `showsRawDevelop`).
    fn raw_develop_sheet(
        develop: RawDevelop,
        session: Entity<EditorSession>,
        host: Arc<dyn SessionHost>,
        weak: WeakEntity<Self>,
        cx: &mut Context<Self>,
    ) -> Entity<RawDevelopSheet> {
        cx.new(|cx| {
            RawDevelopSheet::new(
                session.clone(),
                host.clone(),
                develop.url.clone(),
                develop.settings.clone(),
                Arc::new(move |settings, _window, cx| {
                    let host = host.clone();
                    session.update(cx, |session, _| session.finish_raw_develop(settings, host.as_ref()));
                    let _ = weak.update(cx, |root, cx| {
                        root.raw_develop = None;
                        cx.notify();
                    });
                }),
                cx,
            )
        })
    }

    /// The actions this window answers itself: every command whose Swift body ran at the app level.
    ///
    /// The handlers chain onto the root element, which installs them into the frame's dispatch
    /// tree when it paints — the only phase gpui allows action registration in.
    fn register_actions(root: &Entity<Self>, el: &mut impl InteractiveElement) {
        macro_rules! bind {
            ($action:ty, |$root:ident, $action_arg:ident, $window:ident, $cx:ident| $body:block) => {{
                let entity = root.clone();
                el.interactivity().on_action::<$action>(move |$action_arg, $window, $cx| {
                    let _ = entity.update($cx, |$root, $cx| $body);
                });
            }};
        }
        bind!(actions::NewCanvas, |root, _action, _window, cx| {
            root.new_canvas(cx);
        });
        bind!(actions::OpenProject, |root, _action, window, cx| {
            root.open_project(window, cx);
        });
        bind!(actions::OpenRecentProject, |root, action, window, cx| {
            root.open_recent(action.path.clone(), window, cx);
        });
        bind!(actions::ClearRecentProjects, |root, _action, _window, cx| {
            root.clear_recent(cx);
        });
        bind!(actions::SaveProject, |root, _action, window, cx| {
            root.save(false, window, cx);
        });
        bind!(actions::SaveProjectAs, |root, _action, window, cx| {
            root.save(true, window, cx);
        });
        bind!(actions::ExportPng, |root, _action, window, cx| {
            root.export_png(window, cx);
        });
        bind!(actions::ExportJpeg, |root, _action, window, cx| {
            root.export_jpeg(window, cx);
        });
        bind!(actions::CloseProject, |root, _action, window, cx| {
            root.close_project(window, cx);
        });
        bind!(actions::ImportImages, |root, _action, _window, cx| {
            root.request_import(cx);
        });
        bind!(actions::CheckForUpdates, |root, _action, window, cx| {
            root.check_for_updates(window, cx);
        });
        bind!(actions::ShowKeyboardShortcuts, |root, _action, window, cx| {
            root.show_keyboard_shortcuts(window, cx);
        });
        bind!(actions::CanvasSize, |root, _action, window, cx| {
            root.canvas_size(window, cx);
        });
        bind!(actions::ImageSize, |root, _action, window, cx| {
            root.image_size(window, cx);
        });
        bind!(actions::Trim, |root, _action, window, cx| {
            root.trim(window, cx);
        });
        bind!(actions::GridSettings, |root, _action, window, cx| {
            root.grid_settings(window, cx);
        });
    }
}


// TEMP-PERF: append a timestamped line to one shared timeline file.
fn perf(line: &str) {
    if std::env::var_os("TEMP_PERF").is_none() {
        return;
    }
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("D:/workspace/rust/compositor-rs/target/temp_perf_frames.txt")
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "{now} {line}");
    }
}

impl Render for AppRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // TEMP-PERF
        perf("render_start");
        let min = std::env::var("PERF_MIN").is_ok();
        if std::env::var("PERF_LOOP").is_ok() {
            // TEMP-PERF: force a continuous frame loop, so frame intervals are the frame's cost.
            use std::sync::atomic::{AtomicBool, Ordering};
            static SPAWNED: AtomicBool = AtomicBool::new(false);
            if !SPAWNED.swap(true, Ordering::SeqCst) {
                // TEMP-PERF: with PERF_EDIT the content revision moves without anything else
                // changing, which is what dragging a layer's handle makes the canvas do.
                let edit = std::env::var("PERF_EDIT").is_ok();
                cx.spawn_in(window, async move |this, cx| loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1))
                        .await;
                    let updated = this.update(cx, |root, cx| {
                        if edit {
                            if let Some(session) = root.current_session(cx, false) {
                                session.update(cx, |session, _| {
                                    session.brush_revision = session.brush_revision.wrapping_add(1);
                                });
                            }
                        }
                        cx.notify();
                    });
                    if updated.is_err() {
                        return;
                    }
                })
                .detach();
            }
        }
        self.sync_menus(cx);
        self.sync_sheets(cx);
        self.sync_importer(window, cx);
        self.sync_color_picker(window, cx);
        self.observe_tabs(cx);

        // The shortcuts sheet is centred on the window below its title bar.
        let title_bar = f32::from(gpui_kit::component::TITLE_BAR_HEIGHT);
        let mut panels: Vec<AnyElement> = Vec::new();
        if let Some(panel) = self
            .keyboard_shortcuts
            .render(title_bar, f32::from(window.viewport_size().height) - title_bar, window, cx)
        {
            panels.push(panel);
        }
        let mut sheets: Vec<AnyElement> = Vec::new();
        if let Some(sheet) = self.jpeg_export.clone() {
            sheets.push(sheet.into_any_element());
        }
        if let Some((_, sheet)) = self.psd_conversion.clone() {
            sheets.push(sheet.into_any_element());
        }
        if let Some((_, sheet)) = self.raw_develop.clone() {
            sheets.push(sheet.into_any_element());
        }
        // No card, no overlay: an empty full-window element would sit over the editor.
        let overlay: Option<AnyElement> = if sheets.is_empty() {
            None
        } else {
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(sheets)
                    .into_any_element(),
            )
        };

        let mut root_el = div()
            .id("app-root")
            .relative()
            .size_full()
            .can_drop(|any, _, _| any.is::<ExternalPaths>())
            .on_drop(cx.listener(|root, paths: &ExternalPaths, window, cx| {
                root.receive_paths(paths.paths().to_vec(), window, cx);
            }))
            .child(
                v_flex()
                    .size_full()
                    .child(TitleBar::new().child(self.menu_bar.clone()))
                    .child(div().flex_1().child(self.view.clone()))
                    .children(panels),
            )
            .children(overlay)
            // TEMP-PERF: a zero-size element painted last, to mark where the app's own painting
            // stops and the toolkit's frame ends.
            .child(
                gpui_kit::gpui::canvas(
                    |_, _, _| {},
                    |_, _, _, _| perf("paint_last"),
                )
                .absolute()
                .size(px(0.0)),
            );

        // Action listeners live for one frame; the root element installs them as it paints.
        Self::register_actions(&cx.entity(), &mut root_el);
        commands::register(&mut root_el, cx, self.workspace.clone(), self.host.clone());
        // TEMP-PERF
        perf("render_end");
        if min {
            // TEMP-PERF: a minimal tree, to see whether frame cost tracks the app's own painting.
            {
                use std::sync::atomic::{AtomicBool, Ordering};
                static SPAWNED: AtomicBool = AtomicBool::new(false);
                if !SPAWNED.swap(true, Ordering::SeqCst) {
                    cx.spawn_in(window, async move |this, cx| loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(1))
                            .await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            return;
                        }
                    })
                    .detach();
                }
            }
            return div()
                .size_full()
                .bg(gpui_kit::gpui::black())
                .child(
                    gpui_kit::gpui::canvas(|_, _, _| {}, |_, _, _, _| perf("paint_last"))
                        .absolute()
                        .size(px(0.0)),
                )
                .into_any_element();
        }
        root_el.into_any_element()
    }
}

/// The file name a save panel starts with: the project's stem and the export's extension.
fn suggested_name(project_url: &Option<PathBuf>, extension: &str) -> String {
    let stem = project_url
        .as_ref()
        .and_then(|url| url.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_string());
    format!("{stem}.{extension}")
}
