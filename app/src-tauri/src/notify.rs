//! Notifications for chats you aren't looking at (background chats, #155): one when a chat needs you (an approval
//! waits) and one when its turn ended. Shown only to the person, in Marv's fixed
//! words plus the chat's title and the approval's title; never the model's prose, and never back to a model. There's
//! no answer button: clicking opens the chat, where the whole request is on screen.
//!
//! "Looking" is this chat on screen in a focused window. Turn ends that come close together fold into one
//! notification, so a team of chats finishing at once doesn't stack up.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

/// Turn ends within this of the first are told together.
const FOLD: Duration = Duration::from_secs(3);
/// The event the page opens a chat on, with the chat's id.
pub const OPEN_CHAT: &str = "open-chat";

/// What a chat's event is worth telling the person about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// An approval waits: its title.
    NeedsYou(String),
    /// The turn ended, and how, in Marv's words.
    Finished(&'static str),
}

/// The notice for `event`, if the person isn't `looking` at the chat. A turn the person stopped, or ended by
/// declining, isn't one: they were there.
pub fn notice(event: &Value, looking: bool) -> Option<Notice> {
    if looking {
        return None;
    }
    match event["type"].as_str()? {
        "approval" => Some(Notice::NeedsYou(one_line(event["title"].as_str().unwrap_or(""), 80))),
        "turn_end" => {
            let how = match event["reason"].as_str()? {
                "end" => "finished",
                "max_steps" => "stopped at its step limit",
                "length" => "stopped: the reply hit the model's length limit",
                "loop" => "stopped: it was repeating itself",
                "error" => "stopped on an error",
                _ => return None,
            };
            Some(Notice::Finished(how))
        }
        _ => None,
    }
}

/// `text` as one printable line of at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let line = marv_core::printable(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

/// A notification's summary and body.
pub fn needs_you(chat: &str, title: &str) -> (String, String) {
    let body = if title.is_empty() { "Needs you: an approval is waiting.".to_string() } else { format!("Needs you: {title}") };
    (chat_name(chat), body)
}

/// One notification for the turns that ended together: `(chat title, how)`, in the order they ended.
pub fn finished(ended: &[(String, &'static str)]) -> (String, String) {
    match ended {
        [(chat, how)] => (chat_name(chat), format!("Its turn {how}.")),
        _ => {
            let names: Vec<String> = ended.iter().map(|(chat, _)| chat_name(chat)).collect();
            (format!("{} chats are done", ended.len()), names.join(", "))
        }
    }
}

fn chat_name(title: &str) -> String {
    let name = one_line(title, 60);
    if name.is_empty() { "A Marv chat".to_string() } else { name }
}

/// Turn ends waiting to be told: (chat id, its title, how).
type Ended = Arc<Mutex<Vec<(String, String, &'static str)>>>;

/// Sends the notifications: approvals at once, turn ends folded.
#[derive(Default)]
pub struct Notifier {
    ended: Ended,
}

impl Notifier {
    /// Looks at one of chat `id`'s events; `shown` says whether the page shows the chat now.
    pub fn event(&self, app: &AppHandle, id: &str, title: &str, event: &Value, shown: bool) {
        let looking = shown && focused(app);
        match notice(event, looking) {
            Some(Notice::NeedsYou(what)) => {
                let (summary, body) = needs_you(title, &what);
                show(app, id, &summary, &body);
            }
            Some(Notice::Finished(how)) => {
                let mut ended = self.ended.lock().unwrap_or_else(|e| e.into_inner());
                ended.push((id.to_string(), title.to_string(), how));
                if ended.len() > 1 {
                    return;
                }
                let (app, ended) = (app.clone(), self.ended.clone());
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(FOLD).await;
                    let told = std::mem::take(&mut *ended.lock().unwrap_or_else(|e| e.into_inner()));
                    let Some((first, ..)) = told.first() else { return };
                    let pairs: Vec<(String, &'static str)> = told.iter().map(|(_, t, how)| (t.clone(), *how)).collect();
                    let (summary, body) = finished(&pairs);
                    show(&app, first, &summary, &body);
                });
            }
            None => {}
        }
    }
}

fn focused(app: &AppHandle) -> bool {
    app.get_webview_window("main").and_then(|w| w.is_focused().ok()).unwrap_or(false)
}

/// Shows one notification; a click on it brings the window up on chat `id`. Problems are ignored: a notification is
/// a courtesy, and the sidebar still shows the chat's state.
fn show(app: &AppHandle, id: &str, summary: &str, body: &str) {
    let mut note = notify_rust::Notification::new();
    note.appname("Marv").summary(summary).body(body);
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        note.action("default", "Open").icon("marv-desktop");
        let Ok(handle) = note.show() else { return };
        let (app, id) = (app.clone(), id.to_string());
        // Waits on D-Bus until it's clicked or closed: a thread of its own, never the async runtime's.
        std::thread::spawn(move || {
            handle.wait_for_action(|action| {
                if action == "default" {
                    open(&app, &id);
                }
            })
        });
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        let _ = (app, id);
        let _ = note.show();
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open(app: &AppHandle, id: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    let _ = app.emit(OPEN_CHAT, id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_approval_or_a_turn_end_is_told_only_when_the_person_isnt_looking() {
        let approval = json!({"type": "approval", "id": 3, "title": "Run a command", "command": "cargo publish"});
        assert_eq!(notice(&approval, false), Some(Notice::NeedsYou("Run a command".into())));
        assert_eq!(notice(&approval, true), None);
        let done = json!({"type": "turn_end", "reason": "end"});
        assert_eq!(notice(&done, false), Some(Notice::Finished("finished")));
        assert_eq!(notice(&done, true), None);
        for other in [
            json!({"type": "text_delta", "text": "hi"}),
            json!({"type": "turn_start"}),
            json!({"type": "approval_done", "id": 3}),
        ] {
            assert_eq!(notice(&other, false), None, "{other}");
        }
    }

    #[test]
    fn a_turn_the_person_stopped_or_declined_isnt_told() {
        for reason in ["interrupted", "aborted", "declined", "something new"] {
            assert_eq!(notice(&json!({"type": "turn_end", "reason": reason}), false), None, "{reason}");
        }
        assert_eq!(
            notice(&json!({"type": "turn_end", "reason": "max_steps"}), false),
            Some(Notice::Finished("stopped at its step limit"))
        );
    }

    /// The approval's title and the chat's are cut to one printable line; the rest is Marv's own words, and nothing
    /// else from the event (the command, the diff, the model's text) is used.
    #[test]
    fn the_words_are_marvs_with_one_line_titles() {
        let approval = json!({"type": "approval", "title": "Run\n\u{1b}[31ma command\u{7}", "text": "the model's prose"});
        let Some(Notice::NeedsYou(title)) = notice(&approval, false) else { panic!() };
        assert_eq!(title, "Run a command");
        assert_eq!(needs_you("Fix the\nbuild", &title), ("Fix the build".into(), "Needs you: Run a command".into()));
        assert_eq!(needs_you("", ""), ("A Marv chat".into(), "Needs you: an approval is waiting.".into()));
        let long = "x".repeat(200);
        let Some(Notice::NeedsYou(cut)) = notice(&json!({"type": "approval", "title": long}), false) else { panic!() };
        assert_eq!(cut.chars().count(), 80);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn turns_ending_together_are_one_notification() {
        assert_eq!(finished(&[("api".into(), "finished")]), ("api".into(), "Its turn finished.".into()));
        let three = [("api".into(), "finished"), ("web".into(), "stopped on an error"), (String::new(), "finished")];
        assert_eq!(finished(&three), ("3 chats are done".into(), "api, web, A Marv chat".into()));
    }
}
