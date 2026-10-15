//! The mesh radio, phase 1: presence (marv-rs #124; `docs/specs/2026-10-14-mesh-radio.md`, §5, §6, §8.5, §11).
//! New in marv-rs: Marv (TypeScript) has no radio.
//!
//! Every Marv of this user's that has the radio on shows who else is on it (idle, working, waiting for its user), with
//! a one-line title, without a model in the loop. Phase 1 carries **presence only**: no text from one Marv reaches
//! another's model. `msg`, `request` and the replies are phases 2 and 3.
//!
//! - **Opt-in, off by default.** A [`Radio`] exists for every Marv whose door is open, but it joins, answers joins and
//!   sends presence only while it's on ([`Radio::is_on`]). Only the user's own config (`"mesh": {"radio": true}`) or
//!   the person's `/radio on` turns it on; that's the CLI's business, never a project's, an environment variable's or
//!   a model's.
//! - **Links over the doors (§5.2).** A Marv joining lists the mesh folder ([`crate::discovery::list`]) and calls
//!   [`JOIN`] on every door whose `marv/info` says its radio is on. The far door checks that its radio is on, that the
//!   caller is the process listening on `<from>.sock` (`SO_PEERCRED`, as `marv/wire` does), and that `from` is
//!   another Marv not already linked. The connection then stays open and carries [`FRAME`] notifications both ways.
//!   **One link per pair:** if two Marvs open one at the same moment, the one opened by the name that sorts first is
//!   kept ([`Radio::accept_join`]).
//! - **The sender is the link.** A frame never names its sender: one that has a `from` is refused, and a presence that
//!   names another Marv than the link's is refused ([`parse_frame`]).
//! - **Presence is exactly [`Presence`]'s fields** (§8.5): a frame with any other field is refused. The title is the
//!   conversation's first user message, one printable line of at most [`MAX_TITLE_CHARS`] (empty with titles off);
//!   the project is the folder's last path component only. Presence is only ever shown by the TUI: `mesh_list` gets
//!   names and fixed state words ([`Radio::listed`]), so it never taints.
//! - **Liveness.** A heartbeat every [`RadioTimings::heartbeat`] (15 s), presence coalesced (only the newest waits, at
//!   most one every 2 s). No frame for 45 s: `silent`. For 120 s: the link is closed and the name drops. A link that
//!   ends without a `left` presence is `lost` (the peer crashed or was killed), and the mesh folder is swept at once
//!   (`sweep_dead` under the folder's lock), as it is on each re-list.
//! - **Re-list.** Every 30 s, a Marv on the radio sweeps the folder and joins any door on the radio it has no link to.
//! - **Logged:** joins, leaves, `lost`, `silent`, drops and refused frames go to the trajectory ([`RadioRecord`]),
//!   never every heartbeat.
//! - **Sandbox.** The links run in Marv, on the host. `$XDG_RUNTIME_DIR/marv` is an empty tmpfs in every command's
//!   sandbox, so nothing sandboxed reaches a door. Subagents and `marv -p` have no radio.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use marv_mcp::jsonrpc::{Message, Notification, Request, RequestId, decode_line, encode_line};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::discovery::{DOOR_TIMEOUT, MAX_SESSION_NAME, check_socket, session_socket};
use crate::receptacle::valid_segment;
use crate::switchboard::Switchboard;

/// `marv/radio.join {from, presence}`: another Marv asks for a link (a door request).
pub const JOIN: &str = "marv/radio.join";
/// `marv/radio.frame {…}`: one frame on a link (a notification, both ways).
pub const FRAME: &str = "marv/radio.frame";
/// The frame format's version (`v`).
pub const FRAME_VERSION: u64 = 1;
/// A presence title: the first user message, one line, cut with `…` (as `sessions::title_of`).
pub const MAX_TITLE_CHARS: usize = 56;
/// The project folder's last path component, at most this long.
pub const MAX_PROJECT_CHARS: usize = 32;
/// A presence object, serialized, at most.
pub const MAX_PRESENCE_BYTES: usize = 512;
/// One line on a link, at most: over it, the link is closed (as `MAX_LINE_BYTES`, smaller).
pub const MAX_FRAME_LINE: usize = 8 * 1024;
const MAX_PANE_CHARS: usize = 64;
const MAX_VERSION_CHARS: usize = 32;

/// How often things happen on the radio. [`Default`] is the spec's; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioTimings {
    /// Presence is sent at least this often, changed or not.
    pub heartbeat: Duration,
    /// No frame from a peer for this long: shown `silent`.
    pub silent_after: Duration,
    /// No frame for this long: the link is closed and the name drops.
    pub drop_after: Duration,
    /// A Marv on the radio re-lists the folder this often, sweeping dead sockets and joining doors it missed.
    pub relist: Duration,
    /// A peer that left (or was lost) stays on the roster this long.
    pub left_shown: Duration,
    /// Presence changes coalesce: at most one frame per link this often (heartbeats aside, the newest wins).
    pub presence_gap: Duration,
    /// How often liveness is checked.
    pub tick: Duration,
    /// A link whose frame can't be written for this long is dropped (a peer that stopped reading).
    pub write_timeout: Duration,
}

impl Default for RadioTimings {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(15),
            silent_after: Duration::from_secs(45),
            drop_after: Duration::from_secs(120),
            relist: Duration::from_secs(30),
            left_shown: Duration::from_secs(10),
            presence_gap: Duration::from_secs(2),
            tick: Duration::from_secs(1),
            write_timeout: Duration::from_secs(30),
        }
    }
}

/// What a Marv says it's doing: fixed words (`HerdrState`'s source; `waiting` is an approval waiting for its user).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceState {
    Idle,
    Working,
    Waiting,
    Left,
}

impl PresenceState {
    pub fn as_str(self) -> &'static str {
        match self {
            PresenceState::Idle => "idle",
            PresenceState::Working => "working",
            PresenceState::Waiting => "waiting",
            PresenceState::Left => "left",
        }
    }
}

/// A presence: these fields and no others (§8.5). Never the model, cost, context, tool names, paths, the transcript,
/// the config, the branch or the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Presence {
    /// The mesh name (a checked name, already public in `marv mesh ls`).
    pub name: String,
    pub state: PresenceState,
    /// The conversation's first user message, one printable line, at most [`MAX_TITLE_CHARS`]; empty for a new
    /// conversation or with titles off.
    pub title: String,
    /// The project folder's last path component, at most [`MAX_PROJECT_CHARS`].
    pub project: String,
    /// The Herdr pane id, if any.
    pub pane: Option<String>,
    /// When this state began.
    pub since_ms: u64,
    /// Whether this Marv's wake is on.
    pub wake: bool,
    /// Marv's version.
    pub version: String,
}

/// `text` as one printable line of at most `max` characters, cut with `…`.
fn one_line(text: &str, max: usize) -> String {
    let line = marv_core::printable(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > max { format!("{}…", line.chars().take(max - 1).collect::<String>().trim_end()) } else { line }
}

/// A presence title from the conversation's first user message (`sessions::title_of`'s cut, and `printable()`).
pub fn clean_title(first_message: &str) -> String {
    one_line(first_message, MAX_TITLE_CHARS)
}

/// The project as presence shares it: the folder's last path component only, never the path.
pub fn clean_project(root: &Path) -> String {
    let last = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    one_line(&last, MAX_PROJECT_CHARS)
}

fn plain_word(text: &str, max: usize, extra: &[char]) -> bool {
    !text.is_empty() && text.len() <= max && text.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

impl Presence {
    /// This Marv's presence when it starts: idle, no title yet.
    pub fn new(name: &str, root: &Path, pane: Option<String>, wake: bool) -> Self {
        Self {
            name: name.to_string(),
            state: PresenceState::Idle,
            title: String::new(),
            project: clean_project(root),
            pane: pane.filter(|p| plain_word(p, MAX_PANE_CHARS, &[':', '_', '-', '.'])),
            since_ms: now_ms(),
            wake,
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    /// Whether every field keeps its rule: a checked name, a title and project that are already one clean line in
    /// their limits, plain-word pane and version, and the whole at most [`MAX_PRESENCE_BYTES`].
    pub fn check(&self) -> Result<(), String> {
        if !valid_segment(&self.name) || self.name.len() > MAX_SESSION_NAME {
            return Err("a presence's name isn't a mesh name".into());
        }
        if self.title != clean_title(&self.title) {
            return Err(format!("a presence's title must be one printable line of at most {MAX_TITLE_CHARS} characters"));
        }
        if self.project != one_line(&self.project, MAX_PROJECT_CHARS) || self.project.contains('/') {
            return Err(format!("a presence's project must be a folder's name, at most {MAX_PROJECT_CHARS} characters"));
        }
        if self.pane.as_deref().is_some_and(|p| !plain_word(p, MAX_PANE_CHARS, &[':', '_', '-', '.'])) {
            return Err("a presence's pane isn't a pane id".into());
        }
        if !plain_word(&self.version, MAX_VERSION_CHARS, &['.', '-', '+']) {
            return Err("a presence's version isn't a version".into());
        }
        let size = serde_json::to_string(self).map_or(usize::MAX, |s| s.len());
        if size > MAX_PRESENCE_BYTES {
            return Err(format!("a presence of {size} bytes is over {MAX_PRESENCE_BYTES}"));
        }
        Ok(())
    }

    /// A presence another Marv sent: exactly the listed fields (any other is refused), each keeping its rule.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let presence: Self = serde_json::from_value(value.clone())
            .map_err(|e| format!("not a presence (only name, state, title, project, pane, since_ms, wake, version): {e}"))?;
        presence.check()?;
        Ok(presence)
    }
}

/// `presence` cut to fit [`MAX_PRESENCE_BYTES`]: the title first, then the project, a character at a time (each stays
/// one clean line ending in `…`). 56 characters of four bytes each, with a project like it, are over: unfitted, every
/// peer would refuse this Marv's joins and frames.
fn fitted(mut presence: Presence) -> Presence {
    fn size(p: &Presence) -> usize {
        serde_json::to_string(p).map_or(usize::MAX, |s| s.len())
    }
    fn shorter(text: &str) -> String {
        match text.chars().count() {
            0 | 1 => String::new(),
            n => one_line(text, n - 1),
        }
    }
    while size(&presence) > MAX_PRESENCE_BYTES && !presence.title.is_empty() {
        presence.title = shorter(&presence.title);
    }
    while size(&presence) > MAX_PRESENCE_BYTES && !presence.project.is_empty() {
        presence.project = shorter(&presence.project);
    }
    presence
}

/// A presence frame's fields besides the presence: `{v, id, seq, kind, to, by, hop, at_ms, presence}`. There is no
/// `from`: the receiver knows the sender by the link.
fn presence_frame(own: &str, seq: u64, presence: &Presence) -> Value {
    json!({
        "v": FRAME_VERSION,
        "id": format!("{own}:{seq}"),
        "seq": seq,
        "kind": "presence",
        "to": "*",
        "by": "marv",
        "hop": 0,
        "at_ms": now_ms(),
        "presence": presence,
    })
}

/// Reads one frame that came in on the link from `link_peer`, to this Marv (`own`). Phase 1 takes presence only.
/// Refused (the reason is logged, never shown to a model): a frame that names a sender (`from`: the sender is the
/// link), one not addressed to this Marv, another kind or version, a presence with any field besides the listed ones
/// or breaking their rules, and a presence that names another Marv than the link's.
pub fn parse_frame(params: &Value, own: &str, link_peer: &str) -> Result<Presence, String> {
    let frame = params.as_object().ok_or("a frame is a JSON object")?;
    if frame.contains_key("from") {
        return Err("the frame names its sender: the sender is the link it came in on, never a field".into());
    }
    if frame.get("v").and_then(Value::as_u64) != Some(FRAME_VERSION) {
        return Err("not a version 1 frame".into());
    }
    match frame.get("to") {
        Some(Value::String(all)) if all == "*" => {}
        Some(Value::Array(names)) if names.iter().any(|n| n.as_str() == Some(own)) => {}
        _ => return Err("the frame isn't addressed to this Marv".into()),
    }
    if frame.get("kind").and_then(Value::as_str) != Some("presence") {
        return Err("this Marv's radio carries presence only".into());
    }
    let presence = Presence::parse(frame.get("presence").ok_or("a presence frame without its presence")?)?;
    if presence.name != link_peer {
        return Err("the presence names another Marv than the link's: the sender is the link".into());
    }
    Ok(presence)
}

/// What happened on the radio, for the trajectory (`type: "radio"`). Joins, leaves, `lost`, `silent`, drops and refused
/// frames; never a heartbeat, never a title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RadioRecord {
    pub at_ms: u64,
    /// The other Marv's mesh name.
    pub peer: String,
    pub event: RadioEvent,
    /// Why a frame or join was refused, or a link dropped (Marv's words).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioEvent {
    Joined,
    Left,
    Lost,
    Silent,
    Dropped,
    Refused,
}

/// A peer as the TUI shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterState {
    Idle,
    Working,
    Waiting,
    /// No frame for [`RadioTimings::silent_after`] (greyed).
    Silent,
    /// It said it left; shown for [`RadioTimings::left_shown`].
    Left,
    /// Its link ended without a leave (crashed or killed); shown like `Left`.
    Lost,
}

impl RosterState {
    pub fn as_str(self) -> &'static str {
        match self {
            RosterState::Idle => "idle",
            RosterState::Working => "working",
            RosterState::Waiting => "waiting",
            RosterState::Silent => "silent",
            RosterState::Left => "left",
            RosterState::Lost => "left (lost)",
        }
    }
}

/// One Marv on the roster, for the TUI only (the title never reaches a model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    pub name: String,
    pub state: RosterState,
    pub title: String,
    pub project: String,
    pub pane: Option<String>,
    pub since_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Live,
    Silent,
    Gone { at: Instant, lost: bool },
}

/// A link: its id, who opened it, whether its join was answered, how to stop it and how to tell its writer that this
/// Marv's presence changed.
#[derive(Clone)]
struct Link {
    id: u64,
    opened_by_me: bool,
    established: bool,
    stop: CancellationToken,
    dirty: Arc<Notify>,
}

struct Peer {
    presence: Option<Presence>,
    status: Status,
    last_frame: Instant,
    link: Option<Link>,
}

impl Peer {
    /// A link that's up or being opened (a gone peer's isn't).
    fn linked(&self) -> Option<&Link> {
        self.link.as_ref().filter(|_| !matches!(self.status, Status::Gone { .. }))
    }
}

/// A link the door accepted: what it hands back to the radio with the connection.
pub struct Accepted {
    pub peer: String,
    /// This Marv's presence, the join's answer.
    pub presence: Presence,
    link: Link,
}

/// One Marv's radio. Shared (`Arc`), every method takes `&self`; its tables sit behind short synchronous locks, never
/// held across an await.
pub struct Radio {
    pub name: String,
    dir: PathBuf,
    timings: RadioTimings,
    on: AtomicBool,
    titles: AtomicBool,
    /// This Marv's presence as it is (its title kept even with titles off: [`Self::presence`] leaves it out).
    me: Mutex<Presence>,
    seq: AtomicU64,
    next_link: AtomicU64,
    peers: Mutex<BTreeMap<String, Peer>>,
    changed: tokio::sync::watch::Sender<u64>,
    sb: Weak<Switchboard>,
    stop: CancellationToken,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Drop for Radio {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Radio {
    /// `sb`'s radio, its doors in `dir` (the mesh folder), off until [`Self::turn_on`]. Also set as the switchboard's
    /// radio, so the door answers joins with it and `mesh_list` lists it. Call [`Self::start`] once, in a runtime.
    pub fn new(sb: &Arc<Switchboard>, dir: &Path, me: Presence, timings: RadioTimings) -> Arc<Self> {
        let radio = Arc::new(Self {
            name: sb.session.clone(),
            dir: dir.to_path_buf(),
            timings,
            on: AtomicBool::new(false),
            titles: AtomicBool::new(true),
            me: Mutex::new(Presence { name: sb.session.clone(), ..me }),
            seq: AtomicU64::new(0),
            next_link: AtomicU64::new(0),
            peers: Mutex::default(),
            changed: tokio::sync::watch::channel(0).0,
            sb: Arc::downgrade(sb),
            stop: CancellationToken::new(),
        });
        sb.set_radio(radio.clone());
        radio
    }

    fn peers(&self) -> MutexGuard<'_, BTreeMap<String, Peer>> {
        self.peers.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn me(&self) -> MutexGuard<'_, Presence> {
        self.me.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the liveness ticks and the re-list. Needs a Tokio runtime.
    pub fn start(self: &Arc<Self>) {
        let (me, stop) = (Arc::downgrade(self), self.stop.clone());
        let tick = self.timings.tick;
        tokio::spawn(async move {
            let mut last_relist = Instant::now();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(tick) => {}
                }
                let Some(radio) = me.upgrade() else { return };
                radio.check_liveness();
                if last_relist.elapsed() >= radio.timings.relist {
                    last_relist = Instant::now();
                    if radio.is_on() {
                        tokio::spawn(async move { radio.relist().await });
                    }
                }
            }
        });
    }

    /// Stops everything at once (no leave sent): the process is going.
    pub fn stop(&self) {
        self.stop.cancel();
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    pub fn timings(&self) -> RadioTimings {
        self.timings
    }

    /// Joins: from now on this Marv answers joins, joins every door on the radio and sends presence.
    pub async fn turn_on(self: &Arc<Self>) {
        self.on.store(true, Ordering::SeqCst);
        self.notify_ui();
        self.join_all().await;
    }

    /// Leaves: a `left` presence on every link, then the links close (waits for that a moment, at most a second).
    pub async fn turn_off(&self) {
        self.on.store(false, Ordering::SeqCst);
        let links: Vec<Link> = self.peers().values().filter_map(|p| p.link.clone()).collect();
        for link in &links {
            link.dirty.notify_one();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && links.iter().any(|l| !l.stop.is_cancelled()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for link in links {
            link.stop.cancel();
        }
        self.peers().clear();
        self.notify_ui();
    }

    /// Whether presence carries the conversation's title (`"mesh": {"radio": {"titles": false}}` turns it off).
    pub fn set_titles(&self, on: bool) {
        if self.titles.swap(on, Ordering::SeqCst) != on {
            self.presence_changed();
        }
    }

    /// Changes this Marv's own presence; a change goes to every link (coalesced).
    pub fn update(&self, edit: impl FnOnce(&mut Presence)) {
        let changed = {
            let mut me = self.me();
            let before = me.clone();
            edit(&mut me);
            me.name = self.name.clone();
            if me.state != before.state {
                me.since_ms = now_ms();
            }
            *me != before
        };
        if changed {
            self.presence_changed();
        }
    }

    pub fn set_state(&self, state: PresenceState) {
        self.update(|p| p.state = state);
    }

    /// The title from the conversation's first user message (empty for a new one).
    pub fn set_title(&self, first_message: &str) {
        let title = clean_title(first_message);
        self.update(|p| p.title = title);
    }

    pub fn set_wake(&self, wake: bool) {
        self.update(|p| p.wake = wake);
    }

    fn presence_changed(&self) {
        for peer in self.peers().values() {
            if let Some(link) = peer.linked() {
                link.dirty.notify_one();
            }
        }
    }

    /// This Marv's presence as sent: its title left out when titles are off, and cut to fit
    /// [`MAX_PRESENCE_BYTES`] ([`fitted`]), which every peer checks.
    pub fn presence(&self) -> Presence {
        let mut me = self.me().clone();
        if !self.titles.load(Ordering::SeqCst) {
            me.title.clear();
        }
        fitted(me)
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// A version that changes whenever the roster does: what the TUI waits on.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn notify_ui(&self) {
        self.changed.send_modify(|v| *v += 1);
    }

    fn record(&self, peer: &str, event: RadioEvent, reason: Option<String>) {
        if let Some(sb) = self.sb.upgrade() {
            sb.log_radio(RadioRecord { at_ms: now_ms(), peer: peer.to_string(), event, reason });
        }
    }

    /// Who's on the radio, for the TUI: the linked Marvs with their presence, and those that left a moment ago.
    pub fn roster(&self) -> Vec<RosterEntry> {
        self.peers()
            .iter()
            .filter(|(_, p)| p.link.as_ref().is_some_and(|l| l.established) || matches!(p.status, Status::Gone { .. }))
            .filter_map(|(name, p)| {
                let presence = p.presence.as_ref()?;
                let state = match p.status {
                    Status::Gone { lost: true, .. } => RosterState::Lost,
                    Status::Gone { lost: false, .. } => RosterState::Left,
                    Status::Silent => RosterState::Silent,
                    Status::Live => match presence.state {
                        PresenceState::Idle => RosterState::Idle,
                        PresenceState::Working => RosterState::Working,
                        PresenceState::Waiting => RosterState::Waiting,
                        PresenceState::Left => RosterState::Left,
                    },
                };
                Some(RosterEntry {
                    name: name.clone(),
                    state,
                    title: presence.title.clone(),
                    project: presence.project.clone(),
                    pane: presence.pane.clone(),
                    since_ms: presence.since_ms,
                })
            })
            .collect()
    }

    /// What `mesh_list` may show the model: the linked Marvs' names (checked mesh names), their state (fixed words)
    /// and their Herdr pane as a checked name segment. Never a title or a project: those are someone's words.
    pub fn listed(&self) -> Vec<(String, &'static str, Option<String>)> {
        self.roster()
            .into_iter()
            .filter(|e| !matches!(e.state, RosterState::Left | RosterState::Lost))
            .map(|e| (e.name, e.state.as_str(), e.pane.as_deref().and_then(crate::herdr::pane_segment)))
            .collect()
    }

    /// The links as `(peer, who opened it)`: one per pair.
    pub fn links(&self) -> Vec<(String, String)> {
        self.peers()
            .iter()
            .filter_map(|(name, p)| {
                let link = p.linked().filter(|l| l.established)?;
                Some((name.clone(), if link.opened_by_me { self.name.clone() } else { name.clone() }))
            })
            .collect()
    }

    /// `silent` after [`RadioTimings::silent_after`], dropped after [`RadioTimings::drop_after`]; gone entries leave
    /// the roster after [`RadioTimings::left_shown`].
    fn check_liveness(&self) {
        let mut events = Vec::new();
        let changed = {
            let mut peers = self.peers();
            let before = peers.len();
            let mut changed = false;
            peers.retain(|name, p| {
                let Some(link) = p.link.as_ref().filter(|l| l.established) else {
                    return !matches!(p.status, Status::Gone { at, .. } if at.elapsed() >= self.timings.left_shown);
                };
                if let Status::Gone { at, .. } = p.status {
                    return at.elapsed() < self.timings.left_shown;
                }
                let quiet = p.last_frame.elapsed();
                if quiet >= self.timings.drop_after {
                    link.stop.cancel();
                    events.push((name.clone(), RadioEvent::Dropped, Some(format!("no frame for {} s", quiet.as_secs()))));
                    return false;
                }
                if quiet >= self.timings.silent_after && p.status == Status::Live {
                    p.status = Status::Silent;
                    changed = true;
                    events.push((name.clone(), RadioEvent::Silent, None));
                }
                true
            });
            changed || peers.len() != before
        };
        for (peer, event, reason) in events {
            self.record(&peer, event, reason);
        }
        if changed {
            self.notify_ui();
        }
    }

    /// Sweeps dead sockets (under the folder's lock) and joins every door on the radio this Marv has no link to.
    pub async fn relist(self: &Arc<Self>) {
        let dir = self.dir.clone();
        let _ = tokio::task::spawn_blocking(move || crate::discovery::sweep_locked(&dir)).await;
        self.join_all().await;
    }

    async fn join_all(self: &Arc<Self>) {
        if !self.is_on() {
            return;
        }
        let found = crate::discovery::list(&self.dir).await.unwrap_or_default();
        let joins = found
            .into_iter()
            .filter(|p| p.session != self.name && p.info.radio)
            .filter(|p| !self.peers().get(&p.session).is_some_and(|peer| peer.linked().is_some()))
            .map(|p| Arc::clone(self).join(p.session));
        futures::future::join_all(joins).await;
    }

    /// Opens a link to `peer`'s door. Its answer is its presence; the connection then carries frames both ways.
    pub async fn join(self: Arc<Self>, peer: String) -> Result<(), String> {
        let link = {
            let mut peers = self.peers();
            if !self.is_on() {
                return Err("the radio is off".into());
            }
            if peers.get(&peer).is_some_and(|p| p.linked().is_some()) {
                return Ok(());
            }
            let link = self.new_link(true, false);
            peers.insert(
                peer.clone(),
                Peer { presence: None, status: Status::Live, last_frame: Instant::now(), link: Some(link.clone()) },
            );
            link
        };
        let opened = tokio::time::timeout(DOOR_TIMEOUT, self.open(&peer)).await.unwrap_or_else(|_| Err("no answer".into()));
        let (read, write, presence) = match opened {
            Ok(opened) => opened,
            Err(reason) => {
                let mut peers = self.peers();
                if peers.get(&peer).and_then(|p| p.link.as_ref()).is_some_and(|l| l.id == link.id && !l.established) {
                    peers.remove(&peer);
                }
                return Err(reason);
            }
        };
        {
            let mut peers = self.peers();
            let Some(entry) = peers.get_mut(&peer).filter(|p| p.link.as_ref().is_some_and(|l| l.id == link.id)) else {
                // A join from the other side won the race (the name that sorts first keeps its link): drop this one.
                return Ok(());
            };
            entry.presence = Some(presence);
            entry.last_frame = Instant::now();
            if let Some(l) = entry.link.as_mut() {
                l.established = true;
            }
        }
        self.record(&peer, RadioEvent::Joined, None);
        self.notify_ui();
        self.run_link(peer, link, read, write);
        Ok(())
    }

    fn new_link(&self, opened_by_me: bool, established: bool) -> Link {
        Link {
            id: self.next_link.fetch_add(1, Ordering::SeqCst) + 1,
            opened_by_me,
            established,
            stop: self.stop.child_token(),
            dirty: Arc::new(Notify::new()),
        }
    }

    /// Connects to `peer`'s door and asks to join; returns the connection and the peer's presence.
    async fn open(&self, peer: &str) -> Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf, Presence), String> {
        let socket = session_socket(&self.dir, peer).ok_or("not a mesh name")?;
        check_socket(&socket).map_err(|e| e.to_string())?;
        let stream = UnixStream::connect(&socket).await.map_err(|e| e.to_string())?;
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let params = json!({"from": self.name, "presence": self.presence()});
        let line = encode_line(&Message::Request(Request::new(RequestId::Number(1), JOIN, Some(params))));
        write.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut answer = Vec::new();
        let n = (&mut read).take(MAX_FRAME_LINE as u64 + 1).read_until(b'\n', &mut answer).await.map_err(|e| e.to_string())?;
        if n == 0 || answer.len() > MAX_FRAME_LINE {
            return Err("the other Marv closed the connection".into());
        }
        let Ok(Message::Response(response)) = decode_line(&String::from_utf8_lossy(&answer)) else {
            return Err("the other Marv's answer isn't a join's".into());
        };
        if let Some(err) = response.error {
            return Err(err.message);
        }
        let presence = Presence::parse(response.result.as_ref().and_then(|r| r.get("presence")).unwrap_or(&Value::Null))?;
        if presence.name != peer {
            return Err("the other Marv's presence names another Marv".into());
        }
        Ok((read, write, presence))
    }

    /// The door's side of a join, once it has checked who's calling (`from`, by pid): this Marv's radio is on, `from`
    /// is another Marv with a presence that keeps the rules, and there's no link to it already. When both opened one
    /// at the same moment, the link opened by the name that sorts first is kept.
    pub fn accept_join(&self, from: &str, presence: &Value) -> Result<Accepted, String> {
        if !self.is_on() {
            return Err(format!("{}'s radio is off", self.name));
        }
        if from == self.name || !valid_segment(from) || from.len() > MAX_SESSION_NAME {
            return Err(format!("\"{from}\" isn't another Marv's name"));
        }
        let presence = Presence::parse(presence)?;
        if presence.name != from {
            return Err("the presence names another Marv than the caller".into());
        }
        let link = {
            let mut peers = self.peers();
            if let Some(existing) = peers.get(from).and_then(Peer::linked) {
                if existing.opened_by_me && from < self.name.as_str() {
                    existing.stop.cancel();
                } else {
                    return Err(format!("{from} is already linked to {}", self.name));
                }
            }
            let link = self.new_link(false, true);
            peers.insert(
                from.to_string(),
                Peer { presence: Some(presence), status: Status::Live, last_frame: Instant::now(), link: Some(link.clone()) },
            );
            link
        };
        self.record(from, RadioEvent::Joined, None);
        self.notify_ui();
        Ok(Accepted { peer: from.to_string(), presence: self.presence(), link })
    }

    /// Takes over a door connection whose join was accepted and answered.
    pub fn adopt(self: &Arc<Self>, accepted: Accepted, read: BufReader<OwnedReadHalf>, write: OwnedWriteHalf) {
        self.run_link(accepted.peer, accepted.link, read, write);
    }

    fn run_link(self: &Arc<Self>, peer: String, link: Link, read: BufReader<OwnedReadHalf>, write: OwnedWriteHalf) {
        tokio::spawn(Arc::clone(self).write_loop(peer.clone(), link.clone(), write));
        tokio::spawn(Arc::clone(self).read_loop(peer, link, read));
    }

    async fn send(&self, write: &mut OwnedWriteHalf, presence: &Presence) -> bool {
        let frame = presence_frame(&self.name, self.next_seq(), presence);
        let line = encode_line(&Message::Notification(Notification::new(FRAME, Some(frame))));
        matches!(tokio::time::timeout(self.timings.write_timeout, write.write_all(line.as_bytes())).await, Ok(Ok(())))
    }

    /// Sends this Marv's presence on one link: when it changes (coalesced: the newest, at most one per
    /// [`RadioTimings::presence_gap`]), every heartbeat, and a `left` at once when the radio turns off.
    async fn write_loop(self: Arc<Self>, peer: String, link: Link, mut write: OwnedWriteHalf) {
        // The join carried the presence: that counts as sent.
        let mut last = Instant::now();
        'link: loop {
            if !self.is_on() {
                break;
            }
            tokio::select! {
                _ = link.stop.cancelled() => break,
                _ = link.dirty.notified() => {}
                _ = tokio::time::sleep(self.timings.heartbeat) => {}
            }
            // At most one presence per gap: changes meanwhile are taken by the one sent at its end. A leave goes at once.
            loop {
                if !self.is_on() {
                    break 'link;
                }
                let wait = last + self.timings.presence_gap;
                if Instant::now() >= wait {
                    break;
                }
                tokio::select! {
                    _ = link.stop.cancelled() => break 'link,
                    _ = link.dirty.notified() => {}
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wait)) => break,
                }
            }
            if !self.send(&mut write, &self.presence()).await {
                self.record(&peer, RadioEvent::Dropped, Some("its link stopped taking frames".into()));
                break;
            }
            last = Instant::now();
        }
        if !self.is_on() && !link.stop.is_cancelled() {
            let left = Presence { state: PresenceState::Left, since_ms: now_ms(), ..self.presence() };
            self.send(&mut write, &left).await;
        }
        let _ = write.shutdown().await;
        link.stop.cancel();
    }

    /// Reads frames from one link until it ends; then the peer is `left` (it said so) or `lost`.
    async fn read_loop(self: Arc<Self>, peer: String, link: Link, mut read: BufReader<OwnedReadHalf>) {
        loop {
            let mut line = Vec::new();
            let mut limited = (&mut read).take(MAX_FRAME_LINE as u64 + 1);
            let n = tokio::select! {
                _ = link.stop.cancelled() => break,
                n = limited.read_until(b'\n', &mut line) => n,
            };
            match n {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > MAX_FRAME_LINE => {
                    self.record(&peer, RadioEvent::Refused, Some(format!("a frame line over {MAX_FRAME_LINE} bytes")));
                    break;
                }
                Ok(_) => {}
            }
            let frame = match decode_line(&String::from_utf8_lossy(&line)) {
                Ok(Message::Notification(n)) if n.method == FRAME => n.params.unwrap_or(Value::Null),
                _ => {
                    self.record(&peer, RadioEvent::Refused, Some("not a radio frame".into()));
                    continue;
                }
            };
            self.receive(&peer, &link, &frame);
        }
        link.stop.cancel();
        self.closed(&peer, &link);
    }

    /// One frame from `peer` on `link`: its presence replaces the last, or it's refused and logged.
    fn receive(&self, peer: &str, link: &Link, frame: &Value) {
        let presence = match parse_frame(frame, &self.name, peer) {
            Ok(presence) => presence,
            Err(reason) => return self.record(peer, RadioEvent::Refused, Some(reason)),
        };
        let left = presence.state == PresenceState::Left;
        {
            let mut peers = self.peers();
            let Some(entry) = peers.get_mut(peer).filter(|p| p.link.as_ref().is_some_and(|l| l.id == link.id)) else {
                return;
            };
            if matches!(entry.status, Status::Gone { .. }) {
                return;
            }
            entry.last_frame = Instant::now();
            entry.status = if left { Status::Gone { at: Instant::now(), lost: false } } else { Status::Live };
            entry.presence = Some(presence);
        }
        if left {
            self.record(peer, RadioEvent::Left, None);
            link.stop.cancel();
        }
        self.notify_ui();
    }

    /// A link ended. Without a leave first, the peer is `lost` (crashed or killed): the folder is swept at once, so its
    /// dead door socket goes.
    fn closed(self: &Arc<Self>, peer: &str, link: &Link) {
        let lost = {
            let mut peers = self.peers();
            let Some(entry) = peers.get_mut(peer).filter(|p| p.link.as_ref().is_some_and(|l| l.id == link.id)) else {
                return;
            };
            let lost = !matches!(entry.status, Status::Gone { .. });
            if lost {
                entry.status = Status::Gone { at: Instant::now(), lost: true };
            }
            entry.link = None;
            lost
        };
        if lost && self.is_on() {
            self.record(peer, RadioEvent::Lost, None);
            let dir = self.dir.clone();
            tokio::task::spawn_blocking(move || crate::discovery::sweep_locked(&dir));
        }
        self.notify_ui();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{DoorGuard, DoorInfo, open_door};
    use crate::switchboard::tests::{Log, Runtime};
    use crate::wire::Approver;

    struct NoOne;
    #[async_trait::async_trait]
    impl crate::discovery::PeerApprover for NoOne {
        async fn approve_wire(&self, _request: crate::approval::WireRequest) -> Option<Approver> {
            None
        }
    }

    fn fast() -> RadioTimings {
        RadioTimings {
            heartbeat: Duration::from_secs(60),
            silent_after: Duration::from_secs(60),
            drop_after: Duration::from_secs(120),
            relist: Duration::from_secs(60),
            left_shown: Duration::from_millis(300),
            presence_gap: Duration::from_millis(300),
            tick: Duration::from_millis(20),
            write_timeout: Duration::from_secs(5),
        }
    }

    /// One Marv with its door open and a radio (off).
    struct Marv {
        radio: Arc<Radio>,
        _door: DoorGuard,
        _sb: Arc<Switchboard>,
        _rt: Runtime,
    }

    fn marv(name: &str, mesh: &Path, timings: RadioTimings) -> Marv {
        let rt = Runtime::new();
        let sb = Arc::new(Switchboard::new(name, rt.0.clone(), Arc::new(Log::default())));
        let door =
            open_door(sb.clone(), mesh, DoorInfo { cwd: format!("/work/{name}").into(), pane: None }, Arc::new(NoOne)).unwrap();
        let radio = Radio::new(&sb, mesh, Presence::new(name, Path::new("/work/proj"), None, false), timings);
        radio.start();
        Marv { radio, _door: door, _sb: sb, _rt: rt }
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..500 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn the_timings_are_the_specs() {
        let t = RadioTimings::default();
        assert_eq!(
            (t.heartbeat, t.silent_after, t.drop_after, t.relist, t.left_shown, t.presence_gap),
            (
                Duration::from_secs(15),
                Duration::from_secs(45),
                Duration::from_secs(120),
                Duration::from_secs(30),
                Duration::from_secs(10),
                Duration::from_secs(2)
            )
        );
        assert_eq!((MAX_TITLE_CHARS, MAX_PROJECT_CHARS, MAX_PRESENCE_BYTES, MAX_FRAME_LINE), (56, 32, 512, 8192));
    }

    #[test]
    fn titles_and_projects_are_one_clean_line_in_their_limits() {
        assert_eq!(clean_title("fix the\n\n login\ttest"), "fix the login test");
        assert_eq!(clean_title("\x1b]52;c;aGk=\x07hi \x1b[2Jthere"), "hi there");
        let long = clean_title(&"word ".repeat(30));
        assert!(long.chars().count() <= MAX_TITLE_CHARS && clean_title(&"x".repeat(80)).chars().count() == MAX_TITLE_CHARS);
        assert!(long.ends_with('…'));
        assert_eq!(clean_title(&long), long, "a clean title stays as it is");
        assert_eq!(clean_project(Path::new("/home/me/secret/client-x/api")), "api");
        assert_eq!(clean_project(Path::new(&format!("/x/{}", "p".repeat(40)))).chars().count(), MAX_PROJECT_CHARS);
    }

    #[test]
    fn a_presence_has_exactly_its_fields() {
        let me = Presence::new("alpha", Path::new("/home/me/proj"), Some("w1:p2".into()), true);
        let value = serde_json::to_value(&me).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["name", "pane", "project", "since_ms", "state", "title", "version", "wake"]);
        assert_eq!(Presence::parse(&value).unwrap(), me);
        let mut extra = value.clone();
        extra["cwd"] = json!("/home/me/proj");
        assert!(Presence::parse(&extra).unwrap_err().contains("not a presence"));
        for (key, bad) in [
            ("title", json!("two\nlines")),
            ("title", json!("x".repeat(57))),
            ("title", json!("\x1b[2Jclear")),
            ("project", json!("/home/me/proj")),
            ("name", json!("Not A Name")),
            ("pane", json!("w1 p2; rm")),
            ("version", json!("1.0 (evil)")),
            ("state", json!("hacking")),
        ] {
            let mut v = value.clone();
            v[key] = bad.clone();
            assert!(Presence::parse(&v).is_err(), "{key}: {bad}");
        }
        assert_eq!(Presence::new("a", Path::new("/p"), Some("bad pane!".into()), false).pane, None);
    }

    #[test]
    fn a_frame_is_known_by_its_link_and_carries_presence_only() {
        let p = Presence::new("beta", Path::new("/p"), None, false);
        let frame = presence_frame("beta", 3, &p);
        assert_eq!(frame["id"], "beta:3");
        assert!(frame.get("from").is_none());
        assert_eq!(parse_frame(&frame, "alpha", "beta").unwrap(), p);
        let mut named = frame.clone();
        named["from"] = json!("beta");
        assert!(parse_frame(&named, "alpha", "beta").unwrap_err().contains("names its sender"));
        assert!(parse_frame(&frame, "alpha", "gamma").unwrap_err().contains("another Marv than the link's"));
        let mut elsewhere = frame.clone();
        elsewhere["to"] = json!(["gamma"]);
        assert!(parse_frame(&elsewhere, "alpha", "beta").is_err());
        elsewhere["to"] = json!(["gamma", "alpha"]);
        assert!(parse_frame(&elsewhere, "alpha", "beta").is_ok());
        let mut msg = frame.clone();
        msg["kind"] = json!("msg");
        assert!(parse_frame(&msg, "alpha", "beta").unwrap_err().contains("presence only"));
    }

    #[tokio::test]
    async fn two_marvs_on_the_radio_see_each_other_and_a_leave() {
        let mesh = Runtime::new();
        let (a, b) = (marv("alpha", &mesh.0, fast()), marv("beta", &mesh.0, fast()));
        a.radio.turn_on().await;
        b.radio.turn_on().await;
        until("a link", || a.radio.roster().len() == 1 && b.radio.roster().len() == 1).await;
        assert_eq!(a.radio.links(), [("beta".to_string(), "beta".to_string())], "the newcomer opened it");
        assert_eq!(b.radio.roster()[0].state, RosterState::Idle);
        b.radio.set_state(PresenceState::Working);
        until("working", || a.radio.roster()[0].state == RosterState::Working).await;
        b.radio.turn_off().await;
        until("left", || a.radio.roster().first().is_some_and(|e| e.state == RosterState::Left)).await;
        until("gone after a moment", || a.radio.roster().is_empty()).await;
        assert!(b.radio.roster().is_empty());
    }

    /// Presence coalesces: a burst of changes sends the newest, not each one.
    #[tokio::test]
    async fn presence_coalesces_to_the_newest() {
        let mesh = Runtime::new();
        let (a, b) = (marv("alpha", &mesh.0, fast()), marv("beta", &mesh.0, fast()));
        a.radio.turn_on().await;
        b.radio.turn_on().await;
        until("a link", || a.radio.roster().len() == 1).await;
        let seq_before = b.radio.seq.load(Ordering::SeqCst);
        for i in 0..20 {
            b.radio.set_title(&format!("title {i}"));
        }
        until("the newest title", || a.radio.roster()[0].title == "title 19").await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        let sent = b.radio.seq.load(Ordering::SeqCst) - seq_before;
        assert!((1..=2).contains(&sent), "{sent} frames for 20 changes");
        assert_eq!(a.radio.roster()[0].title, "title 19");
    }

    /// Two Marvs joining each other at the same moment end with one link, the one opened by the name that sorts first.
    #[tokio::test]
    async fn a_simultaneous_join_keeps_one_link_per_pair() {
        for _ in 0..5 {
            let mesh = Runtime::new();
            let (a, b) = (marv("alpha", &mesh.0, fast()), marv("beta", &mesh.0, fast()));
            a.radio.on.store(true, Ordering::SeqCst);
            b.radio.on.store(true, Ordering::SeqCst);
            let (ra, rb) = (a.radio.clone(), b.radio.clone());
            let _ = tokio::join!(ra.join("beta".into()), rb.join("alpha".into()));
            until("one link each way", || a.radio.links().len() == 1 && b.radio.links().len() == 1).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(a.radio.links(), [("beta".to_string(), "alpha".to_string())]);
            assert_eq!(b.radio.links(), [("alpha".to_string(), "alpha".to_string())]);
            assert_eq!(a.radio.roster()[0].state, RosterState::Idle);
        }
    }

    /// A door on the radio that was missed (no join came from it) is found by the re-list.
    #[tokio::test]
    async fn the_relist_joins_a_door_it_missed() {
        let mesh = Runtime::new();
        let a = marv("alpha", &mesh.0, RadioTimings { relist: Duration::from_millis(100), ..fast() });
        a.radio.turn_on().await;
        // beta is on the radio but never joined anyone (as if its join came while the folder's lock was held).
        let b = marv("beta", &mesh.0, fast());
        b.radio.on.store(true, Ordering::SeqCst);
        assert!(a.radio.roster().is_empty());
        until("the re-list's join", || a.radio.roster().len() == 1 && b.radio.roster().len() == 1).await;
        assert_eq!(a.radio.links(), [("beta".to_string(), "alpha".to_string())]);
    }

    /// A title and project of four-byte characters at their limits are over 512 bytes: the presence sent is cut to
    /// fit, so peers don't refuse this Marv.
    #[tokio::test]
    async fn a_presence_is_cut_to_fit_its_size() {
        let mesh = Runtime::new();
        let (a, b) = (marv("alpha", &mesh.0, fast()), marv("beta", &mesh.0, fast()));
        let wide = "\u{1F600}".repeat(80);
        b.radio.update(|p| {
            p.project = clean_project(Path::new(&format!("/x/{wide}")));
            p.pane = Some("p".repeat(MAX_PANE_CHARS));
        });
        b.radio.set_title(&wide);
        assert_eq!(b.radio.me().title.chars().count(), MAX_TITLE_CHARS);
        let sent = b.radio.presence();
        assert_eq!(sent.check(), Ok(()));
        assert!(sent.title.ends_with('…') && sent.title.chars().count() > 20, "{}", sent.title);
        a.radio.turn_on().await;
        b.radio.turn_on().await;
        until("a link", || a.radio.roster().len() == 1).await;
        assert_eq!(a.radio.roster()[0].title, sent.title);
    }

    #[tokio::test]
    async fn titles_off_leaves_the_title_out() {
        let mesh = Runtime::new();
        let (a, b) = (marv("alpha", &mesh.0, fast()), marv("beta", &mesh.0, fast()));
        b.radio.set_title("the secret client's migration");
        b.radio.set_titles(false);
        a.radio.turn_on().await;
        b.radio.turn_on().await;
        until("a link", || a.radio.roster().len() == 1).await;
        assert_eq!(a.radio.roster()[0].title, "");
        b.radio.set_titles(true);
        until("the title", || a.radio.roster()[0].title == "the secret client's migration").await;
    }
}
