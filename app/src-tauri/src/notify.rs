//! Notifications for chats you aren't looking at (background chats, #155): one when a chat needs you (an approval
//! waits) and one when its turn ended. Shown only to the person, and never back to a model. The words are Marv's own
//! plus the chat's title: an approval is named by a fixed label for its tool ("run a command", "change a file"),
//! never by its title, which can carry model-chosen text (a subagent's description, an edit's path, an MCP tool's
//! title). There's no answer button: clicking opens the chat, where the whole request is on screen.
//!
//! "Looking" is this chat on screen in a focused window. Turn ends that come close together fold into one
//! notification (one line per chat), so a team of chats finishing at once doesn't stack up.
//!
//! The body is markup on freedesktop servers that advertise `body-markup`, and notify-rust passes it through as is, so
//! every title in it is escaped (`markup`). Sending never runs on the chat's event loop: each notification is a task
//! of its own (D-Bus calls on Linux are async; elsewhere `show` runs on a blocking thread). An approval's notification
//! is withdrawn once it's answered, and no notification is waited on for longer than `WATCH`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::oneshot;

/// Turn ends within this of the first are told together.
const FOLD: Duration = Duration::from_secs(3);
/// How long a shown notification is watched for a click; then its task ends (the notification may stay on screen).
#[cfg(all(unix, not(target_os = "macos")))]
const WATCH: Duration = Duration::from_secs(60 * 60);
/// The event the page opens a chat on, with the chat's id.
pub const OPEN_CHAT: &str = "open-chat";

/// What a chat's event is worth telling the person about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// An approval waits: its id, and what it asks, in Marv's words ("" when its tool isn't one Marv names).
    NeedsYou(u64, &'static str),
    /// The approval with this id was answered: withdraw its notification.
    Answered(u64),
    /// The turn ended, and how, in Marv's words.
    Finished(&'static str),
}

/// The notice for `event`, if the person isn't `looking` at the chat. A turn the person stopped, or ended by
/// declining, isn't one: they were there. An answered approval is a notice either way (its notification goes).
pub fn notice(event: &Value, looking: bool) -> Option<Notice> {
    let kind = event["type"].as_str()?;
    if kind == "approval_done" {
        return Some(Notice::Answered(event["id"].as_u64()?));
    }
    if looking {
        return None;
    }
    match kind {
        "approval" => Some(Notice::NeedsYou(event["id"].as_u64()?, asks(event["tool"].as_str().unwrap_or("")))),
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

/// What an approval for `tool` asks, as a fixed label: never the request's own title or label.
fn asks(tool: &str) -> &'static str {
    match tool {
        "bash" | "process" => "run a command",
        "write_file" | "edit_file" | "apply_patch" => "change a file",
        "agent" => "start a subagent",
        "web_fetch" => "fetch a web page",
        "github" => "use GitHub",
        "herdr" => "use a terminal pane",
        "mesh_wire" => "connect to another Marv",
        "undo" => "undo changes",
        t if t.starts_with("mcp") => "use an MCP tool",
        _ => "",
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

/// `text` safe in a notification body that a server renders as markup: `&`, `<` and `>` escaped. macOS and Windows
/// show the body as plain text, so there it stays as is (an escape would show).
#[cfg(all(unix, not(target_os = "macos")))]
fn markup(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn markup(text: &str) -> String {
    text.to_string()
}

/// A notification's summary and body for an approval that asks `what` (a label from `asks`).
pub fn needs_you(chat: &str, what: &str) -> (String, String) {
    let body =
        if what.is_empty() { "Needs you: an approval is waiting.".to_string() } else { format!("Needs you: asks to {what}.") };
    (chat_name(chat), body)
}

/// One notification for the turns that ended together: `(chat title, how)`, one per chat, in the order they ended.
pub fn finished(ended: &[(String, &'static str)]) -> (String, String) {
    match ended {
        [(chat, how)] => (chat_name(chat), format!("Its turn {how}.")),
        _ => {
            let names: Vec<String> = ended.iter().map(|(chat, _)| markup(&chat_name(chat))).collect();
            (format!("{} chats are done", ended.len()), names.join(", "))
        }
    }
}

fn chat_name(title: &str) -> String {
    let name = one_line(title, 60);
    if name.is_empty() { "A Marv chat".to_string() } else { name }
}

/// Adds chat `id`'s turn end to the ones waiting to be told: a chat already there keeps its place and takes the
/// latest title and `how`.
fn fold(ended: &mut Vec<(String, String, &'static str)>, id: &str, title: &str, how: &'static str) {
    match ended.iter_mut().find(|(chat, ..)| chat == id) {
        Some(had) => (had.1, had.2) = (title.to_string(), how),
        None => ended.push((id.to_string(), title.to_string(), how)),
    }
}

/// Turn ends waiting to be told: (chat id, its title, how).
type Ended = Arc<Mutex<Vec<(String, String, &'static str)>>>;
/// Approval notifications on screen, by (chat id, approval id): sending on one withdraws it.
type Waiting = Arc<Mutex<HashMap<(String, u64), oneshot::Sender<()>>>>;

/// Sends the notifications: approvals at once, turn ends folded.
#[derive(Default)]
pub struct Notifier {
    ended: Ended,
    waiting: Waiting,
}

impl Notifier {
    /// Looks at one of chat `id`'s events; `shown` says whether the page shows the chat now. Never blocks.
    pub fn event(&self, app: &AppHandle, id: &str, title: &str, event: &Value, shown: bool) {
        let looking = shown && focused(app);
        match notice(event, looking) {
            Some(Notice::NeedsYou(approval, what)) => {
                let (summary, body) = needs_you(title, what);
                let (withdraw, withdrawn) = oneshot::channel();
                let key = (id.to_string(), approval);
                lock(&self.waiting).insert(key.clone(), withdraw);
                let waiting = self.waiting.clone();
                show(app, id, summary, body, withdrawn, move || {
                    lock(&waiting).remove(&key);
                });
            }
            Some(Notice::Answered(approval)) => {
                if let Some(withdraw) = lock(&self.waiting).remove(&(id.to_string(), approval)) {
                    let _ = withdraw.send(());
                }
            }
            Some(Notice::Finished(how)) => {
                let mut ended = lock(&self.ended);
                let first = ended.is_empty();
                fold(&mut ended, id, title, how);
                if !first {
                    return;
                }
                let (app, ended) = (app.clone(), self.ended.clone());
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(FOLD).await;
                    let told = std::mem::take(&mut *lock(&ended));
                    let Some((first, ..)) = told.first() else { return };
                    let pairs: Vec<(String, &'static str)> = told.iter().map(|(_, t, how)| (t.clone(), *how)).collect();
                    let (summary, body) = finished(&pairs);
                    // Nothing withdraws a turn's notification: the sender goes at once, so only a click or `WATCH` ends it.
                    let (_, never) = oneshot::channel();
                    show(&app, first, summary, body, never, || {});
                });
            }
            None => {}
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn focused(app: &AppHandle) -> bool {
    app.get_webview_window("main").and_then(|w| w.is_focused().ok()).unwrap_or(false)
}

/// Shows one notification in a task of its own; a click on it brings the window up on chat `id`, and `withdrawn`
/// firing closes it. `done` runs when the task ends. Problems are ignored: a notification is a courtesy, and the
/// sidebar still shows the chat's state.
#[cfg(all(unix, not(target_os = "macos")))]
fn show(
    app: &AppHandle,
    id: &str,
    summary: String,
    body: String,
    withdrawn: oneshot::Receiver<()>,
    done: impl FnOnce() + Send + 'static,
) {
    let (app, id) = (app.clone(), id.to_string());
    tauri::async_runtime::spawn(async move {
        let mut note = notify_rust::Notification::new();
        note.appname("Marv").summary(&summary).body(&body).action("default", "Open").icon("marv-desktop");
        if let Ok(handle) = note.show_async().await {
            let clicked = handle.wait_for_action_async(|response| {
                if matches!(response, notify_rust::NotificationResponse::Default)
                    || matches!(response, notify_rust::NotificationResponse::Action(a) if a == "default")
                {
                    open(&app, &id);
                }
            });
            let withdraw = async {
                // A dropped sender (nothing will withdraw it) waits for the click or `WATCH` instead.
                if withdrawn.await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = clicked => {}
                () = withdraw => handle.close_async().await,
                () = tokio::time::sleep(WATCH) => {}
            }
        }
        done();
    });
}

/// macOS: the notification is sent when its handle drops, without waiting for a click (clicking one brings the app
/// up). It's sent as the app's bundle (`ai.flologix.marv`), so it shows Marv's name and icon and follows Marv's
/// notification settings, rather than Finder's.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn show(
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] app: &AppHandle,
    _id: &str,
    summary: String,
    body: String,
    _withdrawn: oneshot::Receiver<()>,
    done: impl FnOnce() + Send + 'static,
) {
    #[cfg(target_os = "macos")]
    {
        static BUNDLE: std::sync::Once = std::sync::Once::new();
        let identifier = app.config().identifier.clone();
        BUNDLE.call_once(|| {
            let _ = notify_rust::set_application(&identifier);
        });
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _ = notify_rust::Notification::new().appname("Marv").summary(&summary).body(&body).show();
        done();
    });
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
        let approval = json!({"type": "approval", "id": 3, "tool": "bash", "title": "Run a command", "command": "cargo publish"});
        assert_eq!(notice(&approval, false), Some(Notice::NeedsYou(3, "run a command")));
        assert_eq!(notice(&approval, true), None);
        let done = json!({"type": "turn_end", "reason": "end"});
        assert_eq!(notice(&done, false), Some(Notice::Finished("finished")));
        assert_eq!(notice(&done, true), None);
        for other in [json!({"type": "text_delta", "text": "hi"}), json!({"type": "turn_start"})] {
            assert_eq!(notice(&other, false), None, "{other}");
        }
    }

    /// An answered approval's notification is withdrawn, whether or not the person is looking now.
    #[test]
    fn an_answered_approval_is_withdrawn_either_way() {
        let answered = json!({"type": "approval_done", "id": 3, "decision": "yes"});
        assert_eq!(notice(&answered, false), Some(Notice::Answered(3)));
        assert_eq!(notice(&answered, true), Some(Notice::Answered(3)));
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

    /// An approval is named by a fixed label for its tool: its title (which can carry the model's words, markup or a
    /// link) and everything else in the event never reach the notification. The chat's title is one printable line.
    #[test]
    fn an_approval_is_named_by_its_tool_never_by_its_title() {
        let title = r#"Start x in its own worktree: </b><a href="https://x.example">Update required</a> & more"#;
        for (tool, what) in [
            ("agent", "start a subagent"),
            ("edit_file", "change a file"),
            ("mcp__srv__deploy", "use an MCP tool"),
            ("herdr", "use a terminal pane"),
            ("something new", ""),
        ] {
            let approval = json!({"type": "approval", "id": 1, "tool": tool, "title": title, "text": "the model's prose"});
            assert_eq!(notice(&approval, false), Some(Notice::NeedsYou(1, what)), "{tool}");
            let (summary, body) = needs_you("Fix the\nbuild", what);
            assert_eq!(summary, "Fix the build");
            for leak in ["<", "&", "href", "Update required", "worktree", "prose"] {
                assert!(!body.contains(leak), "{tool}: {body}");
            }
        }
        assert_eq!(needs_you("api", "run a command").1, "Needs you: asks to run a command.");
        assert_eq!(needs_you("", ""), ("A Marv chat".into(), "Needs you: an approval is waiting.".into()));
        let long = "x".repeat(200);
        assert_eq!(chat_name(&long).chars().count(), 60);
        assert!(chat_name(&long).ends_with('…'));
        assert_eq!(chat_name("Run\n\u{1b}[31ma command\u{7}"), "Run a command");
    }

    #[test]
    fn turns_ending_together_are_one_notification() {
        assert_eq!(finished(&[("api".into(), "finished")]), ("api".into(), "Its turn finished.".into()));
        let three = [("api".into(), "finished"), ("web".into(), "stopped on an error"), (String::new(), "finished")];
        assert_eq!(finished(&three), ("3 chats are done".into(), "api, web, A Marv chat".into()));
    }

    /// Chat names in the folded body are escaped: the body is markup on some servers.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn chat_names_in_the_body_are_escaped() {
        let two = [("<b>a</b> & b".into(), "finished"), ("web".into(), "finished")];
        assert_eq!(finished(&two).1, "&lt;b&gt;a&lt;/b&gt; &amp; b, web");
    }

    /// One chat ending twice in a fold is told once, in its first place, with its latest title and how.
    #[test]
    fn a_chat_is_folded_once() {
        let mut ended = Vec::new();
        fold(&mut ended, "1", "api", "finished");
        fold(&mut ended, "2", "web", "finished");
        fold(&mut ended, "1", "api v2", "stopped on an error");
        assert_eq!(
            ended,
            vec![("1".to_string(), "api v2".to_string(), "stopped on an error"), ("2".into(), "web".into(), "finished")]
        );
    }
}
