//! The Keyboard Shortcuts sheet: every remappable shortcut, a recorder for each, and the Save that
//! stores them (port of `KeyboardShortcuts.swift`'s `KeyboardShortcutsSheet`).
//!
//! The table itself is [`crate::shortcuts`]: `ShortcutDefinition::all()`, the three groups
//! ("Menus", "Canvas & Layers", "Text Editing") and the stored overrides in [`ShortcutSettings`].
//! The sheet keeps a draft of the overrides and asks [`ShortcutSettings::problem`] what is wrong
//! with it; nothing is stored until Save.

use std::collections::HashMap;

use crate::shortcuts::{ShortcutChord, ShortcutDefinition, ShortcutSettings, GROUPS};
use crate::widgets::indicatorless_scroll::IndicatorlessScrollView;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::{h_flex, v_flex, Disableable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The sheet's content width (`.frame(width: 660)`).
const WIDTH: f32 = 660.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// The scroll view's height (`.frame(height: 465)`).
const LIST_HEIGHT: f32 = 465.0;
/// The problem line keeps its room (`.frame(height: 22)`), so the buttons do not jump.
const PROBLEM_HEIGHT: f32 = 22.0;
/// The recorder button's size (`.frame(width: 150, height: 26)`).
const RECORDER_WIDTH: f32 = 150.0;
const RECORDER_HEIGHT: f32 = 26.0;

/// What the sheet reports when it closes: Save stored the draft, Cancel (and the panel's close
/// button) left everything as it was.
pub type KeyboardShortcutsFinish = Box<dyn FnOnce(&mut Window, &mut App)>;

/// The sheet's state: the draft overrides, the search, and which row is recording.
pub struct KeyboardShortcutsSheet {
    draft: HashMap<String, ShortcutChord>,
    search: Entity<InputState>,
    recording: Option<String>,
    /// Holds the keys while a row is recording.
    recorder_focus: FocusHandle,
    finish: Option<KeyboardShortcutsFinish>,
    _subscriptions: Vec<Subscription>,
}

impl KeyboardShortcutsSheet {
    /// Creates the sheet and the search field it owns; the caller shows it (the Swift's
    /// `ShortcutSettings.show()` put it in the `keyboardShortcuts` floating panel).
    pub fn open(
        finish: impl FnOnce(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search shortcuts"));
        cx.new(|cx| {
            let mut sheet = Self {
                draft: ShortcutSettings::shared().lock().overrides().clone(),
                search: search.clone(),
                recording: None,
                recorder_focus: cx.focus_handle(),
                finish: Some(Box::new(finish)),
                _subscriptions: Vec::new(),
            };
            sheet._subscriptions.push(cx.subscribe_in(
                &search,
                window,
                |_: &mut Self, _, _: &InputEvent, _, cx| cx.notify(),
            ));
            sheet
        })
    }

    /// The draft the sheet is editing (`draft`).
    pub fn draft(&self) -> &HashMap<String, ShortcutChord> {
        &self.draft
    }

    /// What is wrong with the draft, if anything ([`ShortcutSettings::problem`]).
    pub fn problem(&self) -> Option<String> {
        ShortcutSettings::problem(&self.draft)
    }

    /// Whether Save is available: nothing recording and no problem in the draft.
    pub fn can_save(&self) -> bool {
        self.recording.is_none() && self.problem().is_none()
    }

    /// "Restore Defaults": an empty draft means every chord is the original one.
    pub fn restore_defaults(&mut self, cx: &mut Context<Self>) {
        self.recording = None;
        self.draft.clear();
        cx.notify();
    }

    /// Save: stores the draft and rebinds the app's keys. False (and nothing stored) while the
    /// draft has a problem or a row is still recording.
    pub fn save(&mut self, window: &mut Window, cx: &mut App) -> bool {
        if !self.can_save() {
            return false;
        }
        if !ShortcutSettings::shared().lock().save(self.draft.clone()) {
            return false;
        }
        cx.bind_keys(crate::shortcuts::bindings());
        self.close(window, cx);
        true
    }

    /// Cancel (and the panel's close button): the stored shortcuts stay as they were.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut App) {
        self.close(window, cx);
    }

    /// Hands the outcome to the caller, once.
    fn close(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(finish) = self.finish.take() {
            finish(window, cx);
        }
    }

    /// The chord a row shows: the draft's, or the original when it has not been changed.
    fn chord(&self, definition: &ShortcutDefinition) -> ShortcutChord {
        self.draft
            .get(&definition.id())
            .cloned()
            .unwrap_or_else(|| definition.original.clone())
    }

    /// Starts recording in a row: the recorder holds the keys until one arrives.
    fn start_recording(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.recording = Some(id);
        window.focus(&self.recorder_focus, cx);
        cx.notify();
    }

    /// A key arrived at the sheet: it goes to the recorder, or saves/cancels as the Swift's own
    /// Escape and Return do.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.recording.clone() {
            let chord = ShortcutChord::from_keystroke(&event.keystroke);
            // The Swift's `RecorderButton`: a chord that is not a single key is refused (NSSound.beep).
            if chord.key.chars().count() == 1 {
                self.draft.insert(id, chord);
                self.recording = None;
            }
            cx.notify();
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.cancel(window, cx),
            "enter" => {
                self.save(window, cx);
            }
            _ => {}
        }
    }

    /// One row: the title, and the recorder that shows or takes its chord.
    fn row(&self, definition: &ShortcutDefinition, cx: &mut Context<Self>) -> AnyElement {
        let id = definition.id();
        let recording = self.recording.as_deref() == Some(id.as_str());
        let label = if recording {
            "Press keys…".to_string()
        } else {
            self.chord(definition).label()
        };
        let entity = cx.entity();
        h_flex()
            .items_center()
            .gap(px(8.0))
            .child(div().child(definition.title.clone()))
            .child(div().flex_1())
            .child(
                div()
                    .id(ElementId::Name(format!("shortcut-recorder-{id}").into()))
                    .w(px(RECORDER_WIDTH))
                    .h(px(RECORDER_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 1.0, 0.16))
                    .cursor(CursorStyle::PointingHand)
                    .hover(|style| style.bg(hsla(0.0, 0.0, 1.0, 0.06)))
                    .aria_label(if recording {
                        "Press a shortcut".to_string()
                    } else {
                        label.clone()
                    })
                    .child(div().text_size(px(12.0)).child(label))
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |sheet, cx| sheet.start_recording(id.clone(), window, cx));
                    }),
            )
            .into_any_element()
    }
}

impl Render for KeyboardShortcutsSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.search.read(cx).value().to_string();
        let problem = self.problem();
        let can_save = self.can_save();

        let mut rows: Vec<AnyElement> = Vec::new();
        for group in GROUPS {
            rows.push(
                div()
                    .pt(px(8.0))
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(group)
                    .into_any_element(),
            );
            for definition in ShortcutDefinition::all().iter().filter(|definition| definition.group == group) {
                if !search.is_empty() && !definition.title.to_lowercase().contains(&search.to_lowercase()) {
                    continue;
                }
                rows.push(self.row(definition, cx));
            }
        }
        rows.push(Separator::horizontal().into_any_element());
        rows.push(
            div()
                .pt(px(8.0))
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Contextual keys & mouse gestures")
                .into_any_element(),
        );
        rows.push(
            div()
                .text_size(px(13.0))
                .child(
                    "Text fields keep standard macOS editing keys. Dialogs share the Apply/Cancel \
                     assignments above. Numeric fields use Up/Down, with Shift for larger steps. \
                     Standard macOS commands include Cmd+Q to quit and Ctrl+Cmd+F for full screen. \
                     The shortcut editor itself always uses Return to save and Esc to cancel when \
                     not recording.",
                )
                .into_any_element(),
        );
        rows.push(
            div()
                .text_size(px(13.0))
                .child(
                    "Option temporarily selects the eyedropper in painting tools. Shift constrains \
                     shapes/movement or adds to a selection; Option subtracts from selections or \
                     draws from center. Command-drag moves selected pixels; Command-Option-drag \
                     copies them. Option-drag duplicates layers/folders/effects; Option-click at a \
                     layer boundary toggles clipping. Command-click a thumbnail loads its \
                     selection. Control bypasses snapping. Right-drag adjusts brush size. \
                     Modifier-and-mouse gestures are fixed.",
                )
                .into_any_element(),
        );
        let list = IndicatorlessScrollView::new("keyboard-shortcuts-list")
            .child(v_flex().gap(px(6.0)).children(rows))
            .into_any_element();

        v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(10.0))
            .track_focus(&self.recorder_focus)
            .on_key_down(cx.listener(|sheet, event: &KeyDownEvent, window, cx| {
                sheet.key_down(event, window, cx);
            }))
            .child(
                div()
                    .text_color(hsla(0.0, 0.0, 1.0, 0.6))
                    .child("Click a shortcut, then press its new key combination. Changes apply when you save."),
            )
            .child(Input::new(&self.search).w_full())
            .child(div().h(px(LIST_HEIGHT)).child(list))
            .when_some(problem.clone(), |sheet, problem| {
                sheet.child(
                    div()
                        .h(px(PROBLEM_HEIGHT))
                        .text_size(px(13.0))
                        .text_color(hsla(0.075, 1.0, 0.5, 1.0))
                        .child(problem),
                )
            })
            .child(Separator::horizontal())
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        Button::new("shortcuts-restore-defaults")
                            .label("Restore Defaults")
                            .outline()
                            .on_click(cx.listener(|sheet, _, _, cx| sheet.restore_defaults(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("shortcuts-cancel")
                            .label("Cancel")
                            .outline()
                            .on_click(cx.listener(|sheet, _, window, cx| sheet.cancel(window, cx))),
                    )
                    .child(
                        Button::new("shortcuts-save")
                            .label("Save")
                            .primary()
                            .disabled(!can_save)
                            .on_click(cx.listener(|sheet, _, window, cx| {
                                sheet.save(window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    // Imported explicitly: `use super::*` would re-import gpui-kit's `test` macro and shadow the
    // built-in `#[test]` attribute (the dev-dependency enables `test-support`).
    use crate::shortcuts::ShortcutSettings;
    use std::collections::HashMap;

    #[test]
    fn an_empty_draft_has_no_problem() {
        // The sheet's own rule is `ShortcutSettings::problem`; an untouched draft (all originals)
        // is always valid, which is what Restore Defaults leaves behind.
        assert!(ShortcutSettings::problem(&HashMap::new()).is_none());
    }
}
