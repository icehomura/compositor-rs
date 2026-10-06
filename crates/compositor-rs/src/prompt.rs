//! The replay loop that drives the synchronous `ProjectPrompter` from gpui's asynchronous dialogs.
//!
//! `ProjectController` asks its prompter synchronously (see its module doc: Swift awaited an `NSAlert`
//! or `NSSavePanel`, the port asks the caller and acts on the answer in the same order). gpui's
//! dialogs are asynchronous and run on the foreground executor, so blocking the main thread to wait
//! for one would deadlock. The loop here calls the operation with a prompter that answers from what
//! has already been answered, records anything new and answers the rest with the *abort* answer
//! (`None` / `Replacement::Cancel` / `false`); nothing can mutate on such a pass, because every
//! gating question precedes the mutation in the controller's own order. The recorded questions are
//! then asked for real, in order, and the operation runs again with the answers accumulated, until a
//! pass asks nothing new.
//!
//! `show_error` is not gating: it is recorded and shown after the call returns, and only for the pass
//! that completed (an aborted pass's messages would be about a run that never happened).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use compositor_rs_io::project_controller::{
    ProjectPrompter, Replacement, external_change_alert, save_changes_alert,
};
use futures::channel::oneshot;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{WindowExt as _, h_flex};
use gpui_kit::*;

/// One question the controller asked without an answer, in the order it asked.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Question {
    /// `NSSavePanel` for Save/Save As.
    Destination { suggested_name: String },
    /// `NSOpenPanel` for Open with no file supplied.
    ProjectToOpen,
    /// The unsaved-changes alert (`confirmReplacement`).
    Replacement { project_name: String },
    /// `askToRevert(in:)`.
    Revert { project_name: String },
}

/// The answer to a [`Question`].
#[derive(Clone, Debug, PartialEq)]
enum Answer {
    Destination(Option<PathBuf>),
    ProjectToOpen(Option<PathBuf>),
    Replacement(Replacement),
    Revert(bool),
}

/// The prompter of one pass: the answers so far, the questions still unanswered, and the messages
/// `show_error` handed over.
struct ReplayPrompter {
    answers: Vec<Answer>,
    asked: Vec<Question>,
    errors: Vec<(String, String)>,
}

impl ReplayPrompter {
    fn new(answers: Vec<Answer>) -> Self {
        Self {
            answers,
            asked: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// The next answer, or `None` after recording the question. An answer of the wrong kind (which
    /// the controller's fixed question order cannot produce) is treated as unanswered.
    fn answer(&mut self, question: Question, wanted: fn(Answer) -> Option<Answer>) -> Option<Answer> {
        let Some(answer) = self.answers.get(self.asked.len()).cloned() else {
            self.asked.push(question);
            return None;
        };
        match wanted(answer) {
            Some(answer) => Some(answer),
            None => {
                self.asked.push(question);
                None
            }
        }
    }

    fn take_asked(&mut self) -> Vec<Question> {
        std::mem::take(&mut self.asked)
    }

    fn take_errors(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.errors)
    }
}

impl ProjectPrompter for ReplayPrompter {
    fn choose_project_destination(&mut self, suggested_name: &str, _title: &str) -> Option<PathBuf> {
        let question = Question::Destination {
            suggested_name: suggested_name.to_string(),
        };
        let answer = self.answer(question, |answer| match answer {
            Answer::Destination(path) => Some(Answer::Destination(path)),
            _ => None,
        });
        match answer {
            Some(Answer::Destination(path)) => path,
            _ => None,
        }
    }

    fn choose_project_to_open(&mut self) -> Option<PathBuf> {
        let answer = self.answer(Question::ProjectToOpen, |answer| match answer {
            Answer::ProjectToOpen(path) => Some(Answer::ProjectToOpen(path)),
            _ => None,
        });
        match answer {
            Some(Answer::ProjectToOpen(path)) => path,
            _ => None,
        }
    }

    fn confirm_replacement(&mut self, project_name: &str) -> Replacement {
        let question = Question::Replacement {
            project_name: project_name.to_string(),
        };
        let answer = self.answer(question, |answer| match answer {
            Answer::Replacement(choice) => Some(Answer::Replacement(choice)),
            _ => None,
        });
        match answer {
            Some(Answer::Replacement(choice)) => choice,
            _ => Replacement::Cancel,
        }
    }

    fn ask_to_revert(&mut self, project_name: &str) -> bool {
        let question = Question::Revert {
            project_name: project_name.to_string(),
        };
        let answer = self.answer(question, |answer| match answer {
            Answer::Revert(revert) => Some(Answer::Revert(revert)),
            _ => None,
        });
        match answer {
            Some(Answer::Revert(revert)) => revert,
            _ => false,
        }
    }

    fn show_error(&mut self, title: &str, message: &str) {
        self.errors.push((title.to_string(), message.to_string()));
    }
}

/// Runs one controller operation through the replay loop, then hands its result to `complete`.
///
/// `operation` is called once per pass with a fresh prompter; `complete` runs only for the pass that
/// asked nothing new, i.e. the pass whose result is the operation's own.
pub(crate) fn run<R: 'static>(
    window: &mut Window,
    cx: &mut App,
    mut operation: impl FnMut(&mut Window, &mut App, &mut dyn ProjectPrompter) -> R + 'static,
    mut complete: impl FnMut(R, &mut Window, &mut App) + 'static,
) {
    window.spawn(cx, async move |cx| {
        let mut answers: Vec<Answer> = Vec::new();
        loop {
            let mut prompter = ReplayPrompter::new(answers.clone());
            let pass = cx.update(|window, cx| {
                let result = operation(window, cx, &mut prompter);
                (result, prompter.take_asked(), prompter.take_errors())
            });
            let Ok((result, asked, errors)) = pass else { return };
            if asked.is_empty() {
                for (title, message) in errors {
                    show_error(cx, &title, &message);
                }
                let _ = cx.update(|window, cx| complete(result, window, cx));
                return;
            }
            for question in asked {
                let Some(answer) = ask(cx, question).await else { return };
                answers.push(answer);
            }
        }
    })
    .detach();
}

/// Asks one recorded question for real. `None` means the window went away, which ends the driver.
async fn ask(cx: &mut AsyncWindowContext, question: Question) -> Option<Answer> {
    match question {
        Question::Destination { suggested_name } => {
            let chosen = save_panel(cx, &suggested_name).await;
            Some(Answer::Destination(chosen))
        }
        Question::ProjectToOpen => {
            let options = PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Open".into()),
            };
            let receiver = cx.update(|_window, cx| cx.prompt_for_paths(options)).ok()?;
            let chosen = match receiver.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                Ok(Err(error)) => {
                    log::error!("the open panel failed: {error}");
                    None
                }
                Err(_) => None,
            };
            Some(Answer::ProjectToOpen(chosen))
        }
        Question::Replacement { project_name } => {
            let (title, message) = save_changes_alert(&project_name);
            let choice = replacement_alert(cx, title, message).await.unwrap_or(Replacement::Cancel);
            Some(Answer::Replacement(choice))
        }
        Question::Revert { project_name } => {
            let (title, message) = external_change_alert(&project_name);
            let revert = confirm_alert(cx, title, message, "Revert", "Keep Mine").await.unwrap_or(false);
            Some(Answer::Revert(revert))
        }
    }
}

/// `showError(_:error:)`: the title and the error's message, with the one OK button.
pub(crate) fn show_error(cx: &mut AsyncWindowContext, title: &str, message: &str) {
    let (title, message) = (title.to_string(), message.to_string());
    let _ = cx.update(move |window, cx| report_error(window, cx, &title, &message));
}

/// The same alert for a synchronous caller.
pub(crate) fn report_error(window: &mut Window, cx: &mut App, title: &str, message: &str) {
    let (title, message) = (title.to_string(), message.to_string());
    window.open_alert_dialog(cx, move |alert, _window, _cx| {
        alert
            .title(title.clone())
            .description(message.clone())
            .ok_text("OK")
    });
}

/// `NSSavePanel` for an exported file: the home directory and the suggested name. `None` cancels.
pub(crate) async fn save_panel(cx: &mut AsyncWindowContext, suggested_name: &str) -> Option<PathBuf> {
    let directory = home_directory();
    let receiver = cx
        .update(|_window, cx| cx.prompt_for_new_path(&directory, Some(suggested_name)))
        .ok()?;
    match receiver.await {
        Ok(Ok(path)) => path,
        Ok(Err(error)) => {
            log::error!("the save panel failed: {error}");
            None
        }
        Err(_) => None,
    }
}

/// The unsaved-changes alert's three buttons, as `confirmReplacement` shows them.
async fn replacement_alert(cx: &mut AsyncWindowContext, title: String, message: String) -> Option<Replacement> {
    let (sender, receiver) = oneshot::channel();
    let answer = Rc::new(RefCell::new(Some(sender)));
    let opened = cx.update({
        let answer = answer.clone();
        move |window, cx| {
            window.open_alert_dialog(cx, move |alert, _window, _cx| {
                let save = answer.clone();
                let cancel = answer.clone();
                let discard = answer.clone();
                alert
                    .title(title.clone())
                    .description(message.clone())
                    .footer(
                        h_flex()
                            .justify_center()
                            .gap_2()
                            .child(Button::new("dont-save").label("Don’t Save").on_click(move |_, window, cx| {
                                answer_with(&discard, Replacement::Discard, window, cx);
                            }))
                            .child(Button::new("cancel").label("Cancel").on_click(move |_, window, cx| {
                                answer_with(&cancel, Replacement::Cancel, window, cx);
                            }))
                            .child(Button::new("save").label("Save").primary().on_click(move |_, window, cx| {
                                answer_with(&save, Replacement::Save, window, cx);
                            })),
                    )
            });
        }
    });
    opened.ok()?;
    Some(receiver.await.unwrap_or(Replacement::Cancel))
}

/// The `askToRevert` alert: "Revert" or "Keep Mine".
async fn confirm_alert(
    cx: &mut AsyncWindowContext,
    title: String,
    message: String,
    ok_text: &str,
    cancel_text: &str,
) -> Option<bool> {
    let (sender, receiver) = oneshot::channel();
    let answer = Rc::new(RefCell::new(Some(sender)));
    let (ok_text, cancel_text) = (ok_text.to_string(), cancel_text.to_string());
    let opened = cx.update({
        let answer = answer.clone();
        move |window, cx| {
            window.open_alert_dialog(cx, move |alert, _window, _cx| {
                let ok = answer.clone();
                let cancel = answer.clone();
                alert
                    .confirm()
                    .title(title.clone())
                    .description(message.clone())
                    .ok_text(ok_text.clone())
                    .cancel_text(cancel_text.clone())
                    .on_ok(move |_, _, _| {
                        let _ = ok.borrow_mut().take().map(|sender| sender.send(true));
                        true
                    })
                    .on_cancel(move |_, _, _| {
                        let _ = cancel.borrow_mut().take().map(|sender| sender.send(false));
                        true
                    })
            });
        }
    });
    opened.ok()?;
    Some(receiver.await.unwrap_or(false))
}

/// Records a three-way answer and closes the alert.
fn answer_with(answer: &Rc<RefCell<Option<oneshot::Sender<Replacement>>>>, choice: Replacement, window: &mut Window, cx: &mut App) {
    if let Some(sender) = answer.borrow_mut().take() {
        let _ = sender.send(choice);
    }
    window.close_dialog(cx);
}

/// The initial folder of a save panel: the home directory, as an `NSSavePanel` starts out.
fn home_directory() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
