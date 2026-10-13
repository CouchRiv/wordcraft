//! Ask Agent (gap #25): the person selects text, picks **Ask Agent…** (Help › Agents, the page's
//! context menu, Ctrl+Shift+O / ⌘⇧O) and types an instruction ("shorten this paragraph"). An agent
//! they already run on the same computer, connected through the control channel
//! (`wordcraft-cli mcp --connect`), takes the request with `agent.listen`, does the work with the
//! usual commands and answers with `agent.reply`; the answer shows in the window. WordCraft calls
//! no model and holds no key: the "provider" is whatever agent the person connected.
//!
//! The agent can also send its rewrite as a proposal that streams into the bubble
//! (`agent.reply {proposal}`); once the request is answered the bubble offers Accept, Discard and
//! Retry, for a proposal (the window applies it, a tracked change by the agent) and for the agent's
//! own edits (exactly that request's revisions are accepted or rejected).
//!
//! [`AgentHub`] is the queue between the window and the control channel. The control server's
//! connection threads share it, so `agent.listen` waits there, never on the UI thread; the window
//! reads it every frame and is woken when it changes. See `docs/control-protocol.md` (Ask Agent).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use egui::{Align2, Key, Modifiers, Order, Pos2, Rect, Vec2, pos2, vec2};
use serde_json::{Value, json};
use wordcraft_doc::Pos;

use crate::WordApp;
use crate::agent_actions::{AgentAction, Apply, MENUS};
use crate::theme::Tokens;

/// The shortcut, in the registry's notation (Ctrl+Shift+O; ⌘⇧O on macOS). Word has nothing on it.
pub const SHORTCUT: &str = "Mod+Shift+O";
/// An agent that listened this recently still counts as there, between two `agent.listen`s.
pub const PRESENCE_MS: f64 = 60_000.0;
/// A request an agent took this recently counts as an agent at work.
const WORKING_MS: f64 = 10.0 * 60_000.0;
/// The longest one `agent.listen` waits, whatever it asks for (below the bridge's 90 s read
/// timeout).
pub const MAX_LISTEN_MS: u64 = 60_000;
/// What `agent.listen` waits without a `timeoutMs`.
pub const DEFAULT_LISTEN_MS: u64 = 30_000;
/// Longest instruction and reply kept, in characters; longer ones are cut with "…".
pub const MAX_TEXT_CHARS: usize = 4000;
/// Longest selected text sent with a request, in bytes (`truncated` says when it was cut).
pub const MAX_SELECTION_BYTES: usize = 100_000;
/// Requests kept (answered and cancelled ones are dropped first).
const KEEP: usize = 20;
/// Requests shown in the window at once (the newest).
const SHOWN: usize = 3;
/// Width of the instruction box and the requests card.
const WIDTH: f32 = 340.0;
/// Width for a long answer, which wraps rather than being cut.
const WIDE: f32 = 440.0;

/// Where a request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// In the queue: no agent has taken it yet.
    Waiting,
    /// An agent took it (`agent.listen`) and hasn't closed it.
    Working,
    /// The agent's reply closed it.
    Answered,
    /// The person cancelled it.
    Cancelled,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Working => "working",
            State::Answered => "answered",
            State::Cancelled => "cancelled",
        }
    }
    fn open(self) -> bool {
        matches!(self, State::Waiting | State::Working)
    }
}

/// One request from the window.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: u64,
    /// What the agent is asked: the person's words, or a quick action's instruction.
    pub instruction: String,
    /// The quick action it came from (`agent_actions`), if any.
    pub action: Option<AgentAction>,
    /// What to do with the result.
    pub apply: Apply,
    /// `{text, anchor, focus, truncated}` as it was when the person asked.
    pub selection: Value,
    /// `{title, language}`: the file name (else the document's title, else null) and the
    /// selection's language.
    pub document: Value,
    pub state: State,
    /// The agent's latest text: its answer, or a progress note while it works.
    pub reply: String,
    /// The text the agent proposes for the document (`agent.reply {proposal}`), as it streams it:
    /// the whole proposal so far, replaced at every reply. The window applies it on Accept.
    pub proposal: Option<Proposal>,
    /// The revisions its edits for it made ([`run_for`]): what Accept and Discard resolve.
    pub revisions: Vec<u32>,
    /// The request it asks again (Retry).
    pub retry_of: Option<u64>,
    /// The agent that took it, as it named itself (`agent.listen {agent}`), cleaned up.
    pub agent: Option<String>,
    /// When an agent took it (`now_ms`).
    taken_ms: f64,
    /// When the person asked (`now_ms`), for the time shown while it runs.
    asked_ms: f64,
}

impl Request {
    /// What `agent.listen` hands the agent.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "instruction": self.instruction,
            "action": self.action.as_ref().map(AgentAction::to_json),
            "apply": self.apply.name(),
            "selection": self.selection,
            "document": self.document,
            "review": "trackedChanges",
            // Instead of editing, the agent may send the new text (`agent.reply {proposal}`): the
            // window shows it as it comes and applies it where `apply` says when the person accepts.
            "proposalFormats": if self.apply == Apply::ReplyOnly { json!([]) } else { json!(["text", "markdown"]) },
            "retryOf": self.retry_of,
        })
    }

    /// What the person can accept or discard once it is answered: the agent's proposal, else its
    /// edits; `None` for an answer alone, or while it is open.
    pub fn review(&self) -> Option<Review> {
        if self.state != State::Answered {
            return None;
        }
        if self.proposal.as_ref().is_some_and(|p| !p.text.trim().is_empty()) {
            Some(Review::Proposal)
        } else if !self.revisions.is_empty() {
            Some(Review::Edits)
        } else {
            None
        }
    }

    /// The author of the agent's edits and comments for this request: `Agent (name)`.
    pub fn author(&self) -> String {
        match &self.agent {
            Some(name) => format!("Agent ({name})"),
            None => "Agent".into(),
        }
    }

    /// What the window shows for it: the person's words, or the action's label (English; shown
    /// through the catalog).
    pub fn title(&self) -> &str {
        self.action.as_ref().map_or(self.instruction.as_str(), |a| a.label.as_str())
    }
}

/// What a request's result is, for Accept and Discard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Review {
    /// Text the agent proposed: Accept puts it in the document (a tracked change by the agent),
    /// Discard drops it.
    Proposal,
    /// Tracked changes the agent made: Accept makes them final, Discard rejects them.
    Edits,
}

impl Review {
    pub fn name(self) -> &'static str {
        match self {
            Review::Proposal => "proposal",
            Review::Edits => "edits",
        }
    }
}

/// What an agent proposes to put in the document (`agent.reply {proposal, format}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub text: String,
    /// Markdown (headings, lists, emphasis, tables) rather than plain text, whose lines become
    /// paragraphs.
    pub markdown: bool,
}

impl Proposal {
    /// `proposal` and `format` from `agent.reply`'s params: `None` without a proposal; an error the
    /// agent can act on for anything else than text.
    pub fn from_params(p: &Value) -> Result<Option<Proposal>, String> {
        let markdown = match p.get("format") {
            None | Some(Value::Null) => false,
            Some(f) => match f.as_str() {
                Some("text") => false,
                Some("markdown") => true,
                _ => return Err("`format` is \"text\" or \"markdown\"".into()),
            },
        };
        match p.get("proposal") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(Proposal { text: text.replace("\r\n", "\n"), markdown })),
            Some(_) => Err("`proposal` is the text to put in the document".into()),
        }
    }

    pub fn format(&self) -> &'static str {
        if self.markdown { "markdown" } else { "text" }
    }
}

/// Longest proposal kept, in bytes.
pub const MAX_PROPOSAL_BYTES: usize = 200_000;
/// Most revisions one request keeps (as many as `review.accept {revisions}` takes).
const MAX_REQUEST_REVISIONS: usize = 100_000;
/// A streamed proposal wakes the window at most this often (ms between repaints).
const REPAINT_MS: f64 = 100.0;

/// Longest agent name kept, in characters.
const MAX_AGENT_NAME: usize = 40;

/// An agent's name as it gives it, cleaned for display and for tracked changes: control
/// characters dropped, spaces folded, at most [`MAX_AGENT_NAME`] characters; `None` when nothing
/// is left.
pub fn clean_agent_name(name: &str) -> Option<String> {
    let folded: String = name.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).collect();
    let name = cap_chars(folded.trim(), MAX_AGENT_NAME);
    (!name.is_empty()).then_some(name)
}

#[derive(Default)]
struct Inner {
    requests: VecDeque<Request>,
    next_id: u64,
    /// `agent.listen` calls waiting right now.
    listeners: usize,
    /// When an agent last listened or replied (`now_ms`); 0 = never.
    last_seen_ms: f64,
    /// The name of the agent that last listened, when it gave one.
    last_agent: Option<String>,
    /// When a reply last woke the window (`now_ms`), to pace streamed proposals.
    last_repaint_ms: f64,
}

/// The Ask Agent queue, shared by the window and the control channel's connection threads.
/// Clones share one queue. A poisoned lock is taken over, never a panic.
#[derive(Clone)]
pub struct AgentHub {
    shared: Arc<(Mutex<Inner>, Condvar)>,
    port: u16,
    repaint: Option<egui::Context>,
}

impl AgentHub {
    /// The queue of the control channel on `port`; `repaint` wakes the window when an agent
    /// takes or answers a request.
    pub fn new(port: u16, repaint: Option<egui::Context>) -> Self {
        Self { shared: Arc::new((Mutex::new(Inner { next_id: 1, ..Default::default() }), Condvar::new())), port, repaint }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self) {
        self.shared.1.notify_all();
        if let Some(ctx) = &self.repaint {
            ctx.request_repaint();
        }
    }

    /// Queue a typed request; its id. An instruction that is empty once trimmed is refused.
    pub fn ask(&self, instruction: &str, selection: Value, document: Value) -> Result<u64, String> {
        self.ask_with(instruction, None, selection, document)
    }

    /// Queue a request, from a quick action when `action` is given (its instruction is sent).
    pub fn ask_with(&self, instruction: &str, action: Option<&AgentAction>, selection: Value, document: Value) -> Result<u64, String> {
        let instruction = cap_chars(action.map_or(instruction, |a| a.instruction.as_str()).trim(), MAX_TEXT_CHARS);
        if instruction.is_empty() {
            return Err("type an instruction for the agent first".into());
        }
        let apply = action.map_or(Apply::Auto, |a| a.apply);
        let request = Request {
            id: 0,
            instruction,
            action: action.cloned(),
            apply,
            selection,
            document,
            state: State::Waiting,
            reply: String::new(),
            proposal: None,
            revisions: Vec::new(),
            retry_of: None,
            agent: None,
            taken_ms: 0.0,
            asked_ms: crate::now_ms(),
        };
        Ok(self.queue(request))
    }

    /// Ask again (Retry): request `id`'s instruction or action about the same selection, as a new
    /// request that says which one it retries (`retryOf`); the old one is cancelled if open, and
    /// put away. The new request's id.
    pub fn retry(&self, id: u64) -> Result<u64, String> {
        let old = self.request(id).ok_or_else(|| format!("no Ask Agent request {id}"))?;
        self.cancel(id);
        let new = self.queue(Request {
            id: 0,
            state: State::Waiting,
            reply: String::new(),
            proposal: None,
            revisions: Vec::new(),
            retry_of: Some(id),
            agent: None,
            taken_ms: 0.0,
            asked_ms: crate::now_ms(),
            ..old
        });
        self.dismiss(id);
        Ok(new)
    }

    /// Put `request` in the queue under a new id; the id.
    fn queue(&self, request: Request) -> u64 {
        let id = {
            let mut s = self.lock();
            let id = s.next_id;
            s.next_id = s.next_id.saturating_add(1);
            s.requests.push_back(Request { id, ..request });
            // Drop the oldest closed requests, then (only past twice the limit) the oldest.
            while s.requests.len() > KEEP {
                match s.requests.iter().position(|r| !r.state.open()) {
                    Some(i) => drop(s.requests.remove(i)),
                    None if s.requests.len() > 2 * KEEP => drop(s.requests.pop_front()),
                    None => break,
                }
            }
            id
        };
        self.changed();
        id
    }

    /// Cancel request `id` (waiting or being worked on); false when it was already closed or
    /// isn't there.
    pub fn cancel(&self, id: u64) -> bool {
        let done = {
            let mut s = self.lock();
            match s.requests.iter_mut().find(|r| r.id == id && r.state.open()) {
                Some(r) => {
                    r.state = State::Cancelled;
                    true
                }
                None => false,
            }
        };
        if done {
            self.changed();
        }
        done
    }

    /// Forget a closed request (the person dismissed it in the window).
    pub fn dismiss(&self, id: u64) {
        self.lock().requests.retain(|r| r.id != id || r.state.open());
        self.changed();
    }

    /// Wait up to `timeout` for the oldest waiting request and hand it out (it becomes
    /// `Working`). `alive` is asked before handing one out: a client that has gone gets nothing,
    /// so the request stays for the next agent. Runs on a control-channel connection thread.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn listen(&self, timeout: std::time::Duration, alive: impl Fn() -> bool) -> Option<Request> {
        self.listen_as(timeout, None, alive)
    }

    /// [`AgentHub::listen`] for an agent that names itself (`agent.listen {agent}`): the
    /// request records the name, and the agent's edits for it are tracked under it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn listen_as(&self, timeout: std::time::Duration, agent: Option<&str>, alive: impl Fn() -> bool) -> Option<Request> {
        let agent = agent.and_then(clean_agent_name);
        if agent.is_some() {
            self.lock().last_agent = agent.clone();
        }
        let deadline = std::time::Instant::now() + timeout.min(std::time::Duration::from_millis(MAX_LISTEN_MS));
        let mut s = self.lock();
        s.listeners = s.listeners.saturating_add(1);
        s.last_seen_ms = crate::now_ms();
        drop(s);
        // The window shows that an agent listens.
        if let Some(ctx) = &self.repaint {
            ctx.request_repaint();
        }
        let mut s = self.lock();
        let taken = loop {
            if s.requests.iter().any(|r| r.state == State::Waiting) {
                if !alive() {
                    break None;
                }
                if let Some(r) = s.requests.iter_mut().find(|r| r.state == State::Waiting) {
                    r.state = State::Working;
                    r.taken_ms = crate::now_ms();
                    r.agent = agent.clone();
                    break Some(r.clone());
                }
            }
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero()) else { break None };
            s = self.shared.1.wait_timeout(s, left).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        };
        s.listeners = s.listeners.saturating_sub(1);
        s.last_seen_ms = crate::now_ms();
        drop(s);
        if let Some(ctx) = &self.repaint {
            ctx.request_repaint();
        }
        taken
    }

    /// Put a request an agent took back in the queue (its reply never reached the agent).
    pub fn requeue(&self, id: u64) {
        let mut s = self.lock();
        if let Some(r) = s.requests.iter_mut().find(|r| r.id == id && r.state == State::Working) {
            r.state = State::Waiting;
        }
        drop(s);
        self.changed();
    }

    /// The agent's answer to request `id`: shown in the window. `done` closes the request;
    /// otherwise the text is a progress note and the request stays open. A request the person
    /// cancelled answers `{"cancelled": true}` so the agent can stop.
    pub fn reply(&self, id: u64, text: &str, done: bool) -> Result<Value, String> {
        self.answer(id, Some(text), None, done)
    }

    /// The agent's reply to request `id`: a note or its answer (`text`), a proposal (the whole
    /// text so far, which replaces the last one), or both. `done` closes the request; a proposal
    /// then waits for the person's Accept, Discard or Retry. Streamed replies wake the window at
    /// most every [`REPAINT_MS`], the last one at once. Refused: neither given, a proposal for an
    /// answer-only request, or for one the agent already changed the document for.
    pub fn answer(&self, id: u64, text: Option<&str>, proposal: Option<Proposal>, done: bool) -> Result<Value, String> {
        if text.is_none() && proposal.is_none() {
            return Err("send `text` (a note or your answer), `proposal` (the text to put in the document), or both".into());
        }
        let r = {
            let mut s = self.lock();
            s.last_seen_ms = crate::now_ms();
            match s.requests.iter_mut().find(|r| r.id == id) {
                None => Err(format!("no Ask Agent request {id}: it was never made, or the person dismissed it")),
                Some(r) if r.state == State::Cancelled => Ok(json!({"id": id, "cancelled": true})),
                Some(r) if r.state == State::Answered => Err(format!("Ask Agent request {id} is already answered")),
                Some(r) if proposal.is_some() && r.apply == Apply::ReplyOnly => {
                    Err(format!("Ask Agent request {id} asks for an answer only: send `text`, not a `proposal`"))
                }
                Some(r) if proposal.is_some() && !r.revisions.is_empty() => Err(format!(
                    "Ask Agent request {id} already changed the document (tracked changes, which are its result): answer with `text` only"
                )),
                Some(r) => {
                    if let Some(text) = text {
                        r.reply = cap_chars(text.trim(), MAX_TEXT_CHARS);
                    }
                    if let Some(mut p) = proposal {
                        cap_bytes(&mut p.text, MAX_PROPOSAL_BYTES);
                        r.proposal = Some(p);
                    }
                    r.state = if done { State::Answered } else { State::Working };
                    if r.taken_ms == 0.0 {
                        r.taken_ms = crate::now_ms();
                    }
                    Ok(json!({"id": id, "shown": true, "done": done}))
                }
            }
        };
        self.shared.1.notify_all();
        self.repaint_soon(done);
        r
    }

    /// Wake the window for a reply: at once for the last one, else at most every [`REPAINT_MS`].
    fn repaint_soon(&self, now: bool) {
        let Some(ctx) = &self.repaint else { return };
        let t = crate::now_ms();
        let mut s = self.lock();
        let since = t - s.last_repaint_ms;
        if now || !(0.0..REPAINT_MS).contains(&since) {
            s.last_repaint_ms = t;
            drop(s);
            ctx.request_repaint();
        } else {
            drop(s);
            ctx.request_repaint_after(std::time::Duration::from_millis((REPAINT_MS - since).max(1.0) as u64));
        }
    }

    /// Record revisions an agent's edit for request `id` made (they are what Accept and Discard
    /// resolve).
    pub fn add_revisions(&self, id: u64, revisions: impl IntoIterator<Item = u32>) {
        if let Some(r) = self.lock().requests.iter_mut().find(|r| r.id == id) {
            for rev in revisions {
                if r.revisions.len() < MAX_REQUEST_REVISIONS && !r.revisions.contains(&rev) {
                    r.revisions.push(rev);
                }
            }
        }
    }

    /// The name of the agent that last listened, when it gave one.
    pub fn agent_name(&self) -> Option<String> {
        self.lock().last_agent.clone()
    }

    /// Request `id`, if it is still there.
    pub fn request(&self, id: u64) -> Option<Request> {
        self.lock().requests.iter().find(|r| r.id == id).cloned()
    }

    /// The requests, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.lock().requests.iter().cloned().collect()
    }

    /// Whether an agent is there: one waits in `agent.listen`, listened or replied in the last
    /// [`PRESENCE_MS`], or is working on a request it took in the last ten minutes.
    pub fn agent_present(&self) -> bool {
        let now = crate::now_ms();
        let s = self.lock();
        s.listeners > 0
            || (s.last_seen_ms > 0.0 && now - s.last_seen_ms < PRESENCE_MS)
            || s.requests.iter().any(|r| r.state == State::Working && now - r.taken_ms < WORKING_MS)
    }
}

/// `text` cut to at most `max` characters, with "…" when it was longer.
fn cap_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", text.get(..i).unwrap_or(text)),
        None => text.to_string(),
    }
}

/// `text` cut to at most `max` bytes at a character boundary; whether it was cut.
fn cap_bytes(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// Why Ask Agent can't be used right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The web app has no control channel.
    Web,
    /// The app was started without `--control`.
    NoControlChannel,
    /// Nothing listens on the control channel on this port.
    NoAgent(u16),
}

impl Unavailable {
    /// In English, for scripts and agents (`ui.inspect`, errors).
    pub fn english(self) -> String {
        match self {
            Unavailable::Web => "Ask Agent isn't available in the web app: it needs the desktop app's control channel".into(),
            Unavailable::NoControlChannel => "Ask Agent needs the control channel: start WordCraft with `--control 7981`, then connect your agent with `wordcraft-cli mcp --connect 127.0.0.1:7981`".into(),
            Unavailable::NoAgent(port) => format!(
                "No agent is listening: run `wordcraft-cli mcp --connect 127.0.0.1:{port}` in your agent and ask it to wait for WordCraft requests"
            ),
        }
    }

    /// In the interface language, for the window.
    pub fn shown(self) -> String {
        match self {
            Unavailable::Web => tl!("Ask Agent isn't available in the web app: it needs the desktop app's control channel").to_string(),
            Unavailable::NoControlChannel => tl!("Ask Agent needs the control channel: start WordCraft with `--control 7981`, then connect your agent with `wordcraft-cli mcp --connect 127.0.0.1:7981`").to_string(),
            Unavailable::NoAgent(port) => crate::i18n::fmt(
                tl!("No agent is listening: run `wordcraft-cli mcp --connect 127.0.0.1:{port}` in your agent and ask it to wait for WordCraft requests"),
                &[("port", &port.to_string())],
            ),
        }
    }
}

/// The window's side of Ask Agent.
#[derive(Default)]
pub struct AskAgent {
    /// The control channel's queue: `None` without a control channel (and on the web).
    pub hub: Option<AgentHub>,
    /// The bubble at the selection: the instruction box, then the request it sent.
    pub bubble: Option<Bubble>,
    /// The person's recent instructions, newest first. Kept for this session only: they can
    /// quote the document, so they aren't written to the preferences.
    pub history: Vec<String>,
    /// Where each request's text was when the person asked: where Accept puts a proposal.
    origins: HashMap<u64, Origin>,
}

/// Where a request's text was when the person asked, and how to know it is still there.
#[derive(Clone, Debug)]
struct Origin {
    anchor: Pos,
    focus: Pos,
    /// [`fingerprint`] of the selection then.
    print: u64,
}

impl Origin {
    fn of(s: &wordcraft_engine::Session) -> Self {
        Self { anchor: s.sel.anchor.clone(), focus: s.sel.focus.clone(), print: fingerprint(s) }
    }
}

/// What is selected, as a hash: the selected text, or for a caret its paragraph's text and its
/// place in it. The same before and after means the text a request was about is still there.
fn fingerprint(s: &wordcraft_engine::Session) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if s.sel.is_collapsed() {
        s.doc.para_at(&s.sel.focus).map(|p| p.text.as_str()).hash(&mut h);
        s.sel.focus.off.hash(&mut h);
    } else {
        s.selected_text().hash(&mut h);
    }
    h.finish()
}

/// The bubble at the selection.
pub struct Bubble {
    /// The selection's end when it opened: the bubble stays there while the document scrolls.
    pub anchor: Pos,
    /// What the person types.
    pub text: String,
    /// The request sent from it; `None` while the person types.
    pub request: Option<u64>,
    /// The person pressed a key since the request was sent: the result then doesn't take the
    /// keyboard from them.
    typed: bool,
    /// The request whose result last took the keyboard (Accept is focused once per result).
    reviewed: Option<u64>,
    /// Its buttons held the keyboard last frame (Escape takes the focus away before they are
    /// drawn, so Escape asks this).
    held_keys: bool,
    /// The box has taken the keyboard once.
    focused: bool,
    /// Its size last frame, to place it (a first guess before it is drawn).
    size: Vec2,
}

impl Bubble {
    fn at(anchor: Pos) -> Self {
        Self { anchor, text: String::new(), request: None, typed: false, reviewed: None, held_keys: false, focused: false, size: vec2(WIDTH, 360.0) }
    }
}

impl AskAgent {
    /// Why the person can't ask now, if they can't.
    pub fn unavailable(&self) -> Option<Unavailable> {
        match &self.hub {
            None if cfg!(target_arch = "wasm32") => Some(Unavailable::Web),
            None => Some(Unavailable::NoControlChannel),
            Some(h) if !h.agent_present() => Some(Unavailable::NoAgent(h.port())),
            Some(_) => None,
        }
    }

    /// For `ui.inspect`.
    pub fn inspect(&self) -> Value {
        let requests: Vec<Value> = self
            .hub
            .iter()
            .flat_map(AgentHub::requests)
            .map(|r| {
                json!({
                    "id": r.id, "state": r.state.name(), "action": r.action.as_ref().map(|a| a.id.clone()), "apply": r.apply.name(),
                    "instruction": r.instruction, "agent": r.agent, "reply": r.reply,
                    "proposal": r.proposal.as_ref().map(|p| json!({"text": p.text, "format": p.format()})),
                    "revisions": r.revisions, "review": r.review().map(Review::name), "retryOf": r.retry_of,
                })
            })
            .collect();
        json!({
            "available": self.unavailable().is_none(),
            "reason": self.unavailable().map(Unavailable::english),
            "agentListening": self.hub.as_ref().is_some_and(AgentHub::agent_present),
            "bubble": self.bubble.as_ref().map(|b| json!({"request": b.request})),
            "history": self.history,
            "requests": requests,
            "actions": crate::agent_actions::defaults().iter().map(|a| json!({"id": a.id, "label": a.label, "menu": a.menu, "apply": a.apply.name()})).collect::<Vec<_>>(),
        })
    }
}

/// Recent instructions kept, and shown in the bubble.
const HISTORY: usize = 8;
const HISTORY_SHOWN: usize = 5;

fn remember(history: &mut Vec<String>, text: &str) {
    let text = cap_chars(text.trim(), 300);
    if text.is_empty() {
        return;
    }
    history.retain(|h| *h != text);
    history.insert(0, text);
    history.truncate(HISTORY);
}

/// The selection as an agent gets it.
fn selection(app: &WordApp) -> Value {
    let s = &app.session;
    let mut text = s.selected_text();
    let truncated = cap_bytes(&mut text, MAX_SELECTION_BYTES);
    json!({
        "text": text,
        "anchor": wordcraft_engine::cmd::pos_json(&s.sel.anchor),
        "focus": wordcraft_engine::cmd::pos_json(&s.sel.focus),
        "truncated": truncated,
    })
}

/// The document as an agent gets it: its file name, else its title, else the window's name for a
/// new document (`Document1`, never translated text); whether it is saved to a file; and the
/// language of the selected text (the document's when the selection mixes languages).
fn document(app: &mut WordApp) -> Value {
    let lang = app.session.run("format.state", &json!({})).ok().and_then(|v| v.get("lang").and_then(Value::as_str).map(str::to_string));
    let s = &app.session;
    let title = s
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .or_else(|| (!s.doc.core.title.trim().is_empty()).then(|| s.doc.core.title.clone()))
        .unwrap_or_else(|| "Document1".into());
    json!({"title": title, "saved": s.path.is_some(), "language": lang.or_else(|| s.doc.styles.default_chr.lang.clone())})
}

/// `ui.askAgent`. No params: open the bubble at the selection (refused, with the reason, when no
/// agent is listening). `instruction`: send it without the box; `action`: send that quick action.
/// `cancel`: cancel that request. `close`: close the bubble. `accept`, `discard`, `retry`: what
/// the bubble's buttons do with a request's result ([`accept`], [`discard`], [`retry`]).
pub fn command(app: &mut WordApp, p: &Value) -> Result<Value, String> {
    if let Some(id) = p.get("cancel") {
        let id = id.as_u64().ok_or("`cancel` is a request id")?;
        let cancelled = app.ask_agent.hub.as_ref().is_some_and(|h| h.cancel(id));
        return Ok(json!({"cancelled": cancelled}));
    }
    if p.get("close").and_then(Value::as_bool) == Some(true) {
        app.ask_agent.bubble = None;
        return Ok(json!({}));
    }
    for (key, f) in [("accept", accept as fn(&mut WordApp, u64) -> Result<Value, String>), ("discard", discard), ("retry", retry)] {
        if let Some(id) = p.get(key) {
            let id = id.as_u64().ok_or_else(|| format!("`{key}` is a request id"))?;
            return f(app, id);
        }
    }
    let Some(hub) = app.ask_agent.hub.clone() else {
        let why = app.ask_agent.unavailable().unwrap_or(Unavailable::NoControlChannel);
        app.status(why.shown());
        return Err(why.english());
    };
    if let Some(action) = p.get("action") {
        let id = action.as_str().ok_or("`action` is a quick action's id")?;
        let action =
            crate::agent_actions::find(id).ok_or_else(|| format!("no quick action `{id}` (ui.inspect lists them under askAgent.actions)"))?;
        let (selection, document, origin) = (selection(app), document(app), Origin::of(&app.session));
        let id = hub.ask_with("", Some(&action), selection, document)?;
        app.ask_agent.keep_origin(id, origin);
        return Ok(json!({"id": id}));
    }
    if let Some(text) = p.get("instruction") {
        let text = text.as_str().ok_or("`instruction` is text")?;
        let (selection, document, origin) = (selection(app), document(app), Origin::of(&app.session));
        let id = hub.ask(text, selection, document)?;
        app.ask_agent.keep_origin(id, origin);
        remember(&mut app.ask_agent.history, text);
        return Ok(json!({"id": id}));
    }
    if let Some(why) = app.ask_agent.unavailable() {
        app.status(why.shown());
        return Err(why.english());
    }
    app.ask_agent.bubble = Some(Bubble::at(app.session.sel.ordered().1));
    Ok(json!({"pending": "askAgent"}))
}

/// Run `command` for Ask Agent request `id` (an agent's `engine.execute` with `request`): its
/// edits become tracked changes by the agent ([`Request::author`]), whatever the document's Track
/// Changes setting, which is left alone. Refused once the person cancelled the request or it was
/// answered, and for edits to a request that asks for an answer only (`replyOnly`).
pub fn run_for(app: &mut WordApp, id: u64, command: &str, run: impl FnOnce(&mut WordApp) -> Result<Value, String>) -> Result<Value, String> {
    let request = app.ask_agent.hub.as_ref().and_then(|h| h.request(id)).ok_or_else(|| format!("no Ask Agent request {id}"))?;
    match request.state {
        State::Cancelled => return Err(format!("Ask Agent request {id} was cancelled by the person: `{command}` was not run")),
        State::Answered => return Err(format!("Ask Agent request {id} is already answered: `{command}` was not run")),
        State::Waiting | State::Working => {}
    }
    let mutates = app.session.registry.get(command).is_some_and(|c| c.mutates);
    if mutates && request.apply == Apply::ReplyOnly {
        return Err(format!(
            "Ask Agent request {id} asks for an answer only: the document must not change, so `{command}` was not run; answer with agent.reply"
        ));
    }
    if mutates && request.proposal.is_some() {
        return Err(format!(
            "Ask Agent request {id} has a proposal, which the window puts in the document when the person accepts it: `{command}` was not run; send the whole new text as `proposal` instead"
        ));
    }
    let (r, made) = as_agent(app, request.author(), run);
    if let Some(hub) = &app.ask_agent.hub {
        hub.add_revisions(id, made);
    }
    r
}

/// Run `f` as the agent named `author`: what it changes is a tracked change by that author, in
/// revision records of its own ([`wordcraft_engine::Session::revision_base`]); the person's name
/// and Track Changes setting are left as they were. `f`'s result, and the revisions it made.
fn as_agent<T>(app: &mut WordApp, author: String, f: impl FnOnce(&mut WordApp) -> T) -> (T, std::ops::Range<u32>) {
    let base = app.session.doc.revisions.len();
    let author = std::mem::replace(&mut app.session.author, author);
    let forced = std::mem::replace(&mut app.session.force_tracking, true);
    let floor = std::mem::replace(&mut app.session.revision_base, base);
    let r = f(app);
    app.session.author = author;
    app.session.force_tracking = forced;
    app.session.revision_base = floor;
    let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    (r, to_u32(base)..to_u32(app.session.doc.revisions.len()).max(to_u32(base)))
}

impl AskAgent {
    /// Remember where request `id`'s text was; forget requests that are gone.
    fn keep_origin(&mut self, id: u64, origin: Origin) {
        self.origins.insert(id, origin);
        if self.origins.len() > 2 * KEEP
            && let Some(hub) = &self.hub
        {
            let live: Vec<u64> = hub.requests().iter().map(|r| r.id).collect();
            self.origins.retain(|id, _| live.contains(id));
        }
    }

    /// The bubble no longer follows request `id` (it was accepted, discarded or retried).
    fn finished(&mut self, id: u64, next: Option<u64>) {
        if let Some(b) = self.bubble.as_mut().filter(|b| b.request == Some(id)) {
            match next {
                Some(next) => {
                    b.request = Some(next);
                    b.typed = false;
                }
                None => self.bubble = None,
            }
        }
        self.origins.remove(&id);
    }
}

/// Accept request `id`'s result. A proposal goes in the document where the person asked: in
/// place of the text they had selected (`replace`, typed requests), or after its paragraph
/// (`insertBelow`), as one Undo step and a tracked change by the agent, wherever the caret is now.
/// The text must still be there: at the same place, else found again when it is in one paragraph
/// and only once in the document; otherwise nothing changes and the person is asked to select it
/// again. The agent's own edits are made final instead (`review.accept {revisions}`, one Undo
/// step); another author's changes, or another request's, are never touched.
pub fn accept(app: &mut WordApp, id: u64) -> Result<Value, String> {
    let hub = app.ask_agent.hub.clone().ok_or("no Ask Agent queue")?;
    let r = hub.request(id).ok_or_else(|| format!("no Ask Agent request {id}"))?;
    if r.state.open() {
        return Err(format!("Ask Agent request {id} is still being worked on: wait for the agent's answer, or cancel it"));
    }
    let author = r.author();
    let result = match (r.review(), &r.proposal) {
        (Some(Review::Proposal), Some(p)) => {
            apply_proposal(app, &r, p)?;
            app.status(crate::i18n::fmt(tl!("Accepted: the change is tracked as {author}."), &[("author", &author)]));
            json!({"accepted": id, "review": "proposal", "author": author})
        }
        (Some(Review::Edits), _) => {
            let resolved = resolve_edits(app, &r, true)?;
            if resolved {
                app.status(crate::i18n::fmt(tl!("Accepted the changes by {author}."), &[("author", &author)]));
            } else {
                app.status(tl!("Nothing left to accept: the agent's changes were already accepted, rejected or undone.").to_string());
            }
            json!({"accepted": id, "review": "edits", "author": author, "resolved": resolved})
        }
        _ => return Err(format!("Ask Agent request {id} has nothing to accept: no proposal and no change in the document")),
    };
    hub.dismiss(id);
    app.ask_agent.finished(id, None);
    app.canvas.want_focus = true;
    Ok(result)
}

/// Discard request `id`'s result: drop its proposal, or reject the agent's edits for it (one Undo
/// step); a request still open is cancelled first. The request is put away.
pub fn discard(app: &mut WordApp, id: u64) -> Result<Value, String> {
    let hub = app.ask_agent.hub.clone().ok_or("no Ask Agent queue")?;
    let r = hub.request(id).ok_or_else(|| format!("no Ask Agent request {id}"))?;
    hub.cancel(id);
    let rejected = r.proposal.is_none() && !r.revisions.is_empty() && resolve_edits(app, &r, false)?;
    hub.dismiss(id);
    app.ask_agent.finished(id, None);
    app.canvas.want_focus = true;
    if rejected {
        app.status(crate::i18n::fmt(tl!("Discarded: the changes by {author} are rejected."), &[("author", &r.author())]));
    }
    Ok(json!({"discarded": id, "rejected": rejected}))
}

/// Ask again: the agent's edits for request `id` are rejected (one Undo step), then the same
/// instruction or quick action about the same text goes out as a new request (`retryOf`), which
/// the bubble follows. Its id.
pub fn retry(app: &mut WordApp, id: u64) -> Result<Value, String> {
    let hub = app.ask_agent.hub.clone().ok_or("no Ask Agent queue")?;
    let r = hub.request(id).ok_or_else(|| format!("no Ask Agent request {id}"))?;
    hub.cancel(id);
    if r.proposal.is_none() && !r.revisions.is_empty() {
        resolve_edits(app, &r, false)?;
    }
    let new = hub.retry(id)?;
    if let Some(origin) = app.ask_agent.origins.get(&id).cloned() {
        app.ask_agent.origins.insert(new, origin);
    }
    app.ask_agent.finished(id, Some(new));
    app.canvas.want_focus = true;
    Ok(json!({"id": new, "retryOf": id}))
}

/// Accept or reject the agent's edits for request `r` (its revisions still by its author); false
/// when none is left (accepted, rejected or undone already).
fn resolve_edits(app: &mut WordApp, r: &Request, accept: bool) -> Result<bool, String> {
    let author = r.author();
    let ids: Vec<u32> =
        r.revisions.iter().copied().filter(|i| app.session.doc.revisions.get(*i as usize).is_some_and(|rev| rev.author == author)).collect();
    if ids.is_empty() {
        return Ok(false);
    }
    match app.execute(if accept { "review.accept" } else { "review.reject" }, json!({"revisions": ids})) {
        Ok(_) => Ok(true),
        // None of them is in the document any more.
        Err(e) if e.contains("none of these revisions") => Ok(false),
        Err(e) => Err(e),
    }
}

/// Put `p`, request `r`'s proposal, in the document where the person asked (see [`accept`]). On
/// failure the document, the selection and Undo are as they were.
fn apply_proposal(app: &mut WordApp, r: &Request, p: &Proposal) -> Result<(), String> {
    let snap = app.session.edit_snapshot();
    let found = app.ask_agent.origins.get(&r.id).cloned().is_some_and(|o| locate(app, r, &o));
    if !found {
        app.session.restore(snap);
        // English for scripts; the window shows it through the catalogs (`review_act`).
        return Err("The text this request was about has changed since: select it again and ask again.".into());
    }
    // Plain text takes the formatting where it lands; Markdown brings its own (lists, headings).
    let (paste, params) = if p.markdown { ("edit.paste", json!({"markdown": p.text})) } else { ("edit.pasteMerge", json!({"text": p.text})) };
    let below = r.apply == Apply::InsertBelow;
    let (done, _) = as_agent(app, r.author(), |app| -> Result<(), String> {
        if below {
            let (_, b) = app.session.sel.ordered();
            let end = app.session.doc.para_at(&b).map_or(b.off, |para| para.len());
            app.execute("caret.set", json!({"pos": wordcraft_engine::cmd::pos_json(&Pos { off: end, ..b })}))?;
            app.execute("text.newParagraph", json!({}))?;
            // The new paragraph and the text in it are one Undo step.
            app.session.join_next_undo();
        }
        app.execute(paste, params).map(drop)
    });
    if done.is_err() {
        app.session.restore(snap);
    }
    done
}

/// Select the text request `r` was about: where it was if it is unchanged, else the one place in
/// the document with that text (in one paragraph of the body). Whether it was found.
fn locate(app: &mut WordApp, r: &Request, o: &Origin) -> bool {
    let range = json!({"anchor": wordcraft_engine::cmd::pos_json(&o.anchor), "focus": wordcraft_engine::cmd::pos_json(&o.focus)});
    if app.session.run("select.range", &range).is_ok() && fingerprint(&app.session) == o.print {
        return true;
    }
    let text = r.selection.get("text").and_then(Value::as_str).unwrap_or("");
    let whole = r.selection.get("truncated").and_then(Value::as_bool) != Some(true);
    if text.is_empty() || !whole || text.contains('\n') || o.anchor.story != wordcraft_doc::StoryRef::Body {
        return false;
    }
    let unique = app.session.run("select.text", &json!({"text": text, "occurrence": 2})).is_err()
        && app.session.run("select.text", &json!({"text": text})).is_ok();
    unique && fingerprint(&app.session) == o.print
}

/// Send what is in the box; the bubble then follows the request.
fn send(app: &mut WordApp) {
    let Some(text) = app.ask_agent.bubble.as_ref().map(|b| b.text.clone()) else { return };
    match command(app, &json!({"instruction": text})) {
        Ok(v) => {
            if let Some(b) = app.ask_agent.bubble.as_mut() {
                b.request = v.get("id").and_then(Value::as_u64);
                b.text.clear();
            }
            app.canvas.want_focus = true;
        }
        Err(e) => app.status(tl!(&e).to_string()),
    }
}

/// Send a quick action from the bubble; the bubble then follows the request.
fn send_action(app: &mut WordApp, action: &str) {
    match command(app, &json!({"action": action})) {
        Ok(v) => {
            if let Some(b) = app.ask_agent.bubble.as_mut() {
                b.request = v.get("id").and_then(Value::as_u64);
                b.text.clear();
            }
            app.canvas.want_focus = true;
        }
        Err(e) => app.status(tl!(&e).to_string()),
    }
}

/// Where the bubble goes: below the line at `anchor` (the selection's end), inside the canvas;
/// above the line when there is no room below; the canvas's corner when the anchor isn't on
/// screen. Pure geometry; `None` for a degenerate canvas.
pub fn bubble_pos(anchor: Option<(Pos2, f32)>, size: Vec2, viewport: Rect) -> Option<Pos2> {
    if !viewport.is_finite() || !viewport.is_positive() {
        return None;
    }
    let (at, h) = anchor.filter(|(p, h)| p.is_finite() && h.is_finite()).unwrap_or((viewport.min + vec2(24.0, 24.0), 0.0));
    let x = at.x.clamp(viewport.left(), (viewport.right() - size.x).max(viewport.left()));
    let below = at.y + h + GAP;
    let y = if below + size.y <= viewport.bottom() { below } else { (at.y - size.y - GAP).max(viewport.top()) };
    Some(pos2(x, y))
}

/// Where the chip goes: beside the mini toolbar (`bar`) when it is up, so the two float together
/// above the selection and cover no text; otherwise where the bar would be, above the selection's
/// first line (`first_line`), or under its end when there is no room above. Always inside the
/// canvas. Pure geometry; `None` for degenerate input.
pub fn chip_pos(first_line: Rect, end: Option<(Pos2, f32)>, size: Vec2, viewport: Rect, bar: Option<Rect>) -> Option<Pos2> {
    if !viewport.is_finite() || !viewport.is_positive() || !first_line.is_finite() {
        return None;
    }
    let fits = |p: Pos2| viewport.contains_rect(Rect::from_min_size(p, size));
    let clamp_x = |x: f32| x.clamp(viewport.left(), (viewport.right() - size.x).max(viewport.left()));
    if let Some(bar) = bar.filter(|b| b.is_finite()) {
        let right = pos2(bar.right() + GAP, bar.top());
        let left = pos2(bar.left() - GAP - size.x, bar.top());
        return Some(match (fits(right), fits(left)) {
            (true, _) => right,
            (false, true) => left,
            _ => pos2(clamp_x(bar.left()), (bar.bottom() + GAP).min(viewport.bottom() - size.y).max(viewport.top())),
        });
    }
    let above = pos2(clamp_x(first_line.left()), first_line.top() - GAP - size.y);
    if above.y >= viewport.top() {
        return Some(above);
    }
    bubble_pos(end.or(Some((first_line.left_top(), first_line.height()))), size, viewport)
}

/// Space between the selection and the chip or bubble.
const GAP: f32 = 6.0;
const CHIP_H: f32 = 24.0;

/// Where the chip was drawn last frame, kept in the context's memory for tests.
pub fn chip_id() -> egui::Id {
    egui::Id::new("wc_ask_agent_chip").with("shown")
}

/// Whether the chip shows: on a selection, while the mini toolbar may show (same rules: not on a
/// caret, not over a dialog, the Backstage or the context menu, not while dragging), the bubble
/// is closed and an agent is listening (nothing floats otherwise).
pub fn chip_visible(app: &WordApp) -> bool {
    app.ask_agent.bubble.is_none() && app.ask_agent.hub.is_some() && crate::mini_toolbar::visible(app) && app.ask_agent.unavailable().is_none()
}

/// The chip on the selection, the bubble, and the card with the other requests in the canvas's
/// bottom-right corner. `viewport` is the canvas area.
pub fn show(app: &mut WordApp, ctx: &egui::Context, viewport: Rect) {
    if !viewport.is_finite() || !viewport.is_positive() {
        return;
    }
    show_chip(app, ctx, viewport);
    let shown = show_bubble(app, ctx, viewport);
    show_requests(app, ctx, viewport, shown);
}

fn show_chip(app: &mut WordApp, ctx: &egui::Context, viewport: Rect) {
    if !chip_visible(app) {
        ctx.data_mut(|d| d.remove::<Rect>(chip_id()));
        return;
    }
    let Some(first_line) = app.canvas.mini_anchor else { return };
    let end = app.session.sel.ordered().1;
    let end = crate::canvas::pos_screen(app, &end);
    let t = Tokens::get(ctx);
    let label = tl!("Ask Agent").to_string();
    let font = crate::theme::regular(12.0);
    let text_w = ctx.fonts_mut(|f| f.layout_no_wrap(label.clone(), font.clone(), t.text).size().x);
    let size = vec2(text_w + 34.0, CHIP_H);
    // The mini toolbar is drawn this frame exactly when it is on and may show (`chip_visible`
    // already asked `mini_toolbar::visible`).
    let bar = app.ui.mini_toolbar.then(|| crate::mini_toolbar::anchor_rect(first_line.min.y, first_line.min.x, viewport)).flatten();
    let Some(pos) = chip_pos(first_line, end, size, viewport, bar) else { return };
    let mut clicked = false;
    egui::Area::new(egui::Id::new("wc_ask_agent_chip")).order(Order::Middle).fixed_pos(pos).show(ctx, |ui| {
        let (r, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        let p = ui.painter();
        p.rect_filled(r.translate(vec2(0.0, 1.5)), egui::CornerRadius::same(6), t.page_shadow);
        p.rect_filled(r, egui::CornerRadius::same(6), if resp.hovered() { t.hover } else { t.menu });
        p.rect_stroke(r, egui::CornerRadius::same(6), egui::Stroke::new(1.0, t.border_strong), egui::StrokeKind::Inside);
        let icon = Rect::from_center_size(pos2(r.min.x + 14.0, r.center().y), vec2(16.0, 16.0));
        crate::icons::paint(p, icon, "askAgent", t.icon, t.accent);
        p.text(pos2(r.min.x + 26.0, r.center().y), Align2::LEFT_CENTER, &label, font.clone(), t.text);
        let sc = crate::widgets::shortcut_text(app, "ui.askAgent");
        let resp = resp.on_hover_text(format!("{label} ({sc})"));
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
        clicked = resp.clicked();
        ctx.data_mut(|d| d.insert_temp(chip_id(), r));
    });
    if clicked {
        let _ = app.run("ui.askAgent", json!({}));
    }
}

/// The bubble; the request it shows, if any.
fn show_bubble(app: &mut WordApp, ctx: &egui::Context, viewport: Rect) -> Option<u64> {
    let anchor = app.ask_agent.bubble.as_ref()?.anchor.clone();
    let hub = app.ask_agent.hub.clone();
    let request = app.ask_agent.bubble.as_ref().and_then(|b| b.request);
    let current = request.and_then(|id| hub.as_ref()?.requests().into_iter().find(|r| r.id == id));
    // Its request was dismissed (from the corner card, or by a script): nothing left to show.
    if request.is_some() && current.is_none() {
        app.ask_agent.bubble = None;
        return None;
    }
    let at = crate::canvas::pos_screen(app, &anchor);
    let width = if current.as_ref().is_some_and(|r| r.reply.chars().count() > 160 || r.proposal.is_some()) { WIDE } else { WIDTH };
    // A key the person presses while the agent works: its result then won't take the keyboard.
    if current.as_ref().is_some_and(|r| r.state.open())
        && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. } | egui::Event::Text(_))))
        && let Some(b) = app.ask_agent.bubble.as_mut()
    {
        b.typed = true;
    }
    // A result to review takes the keyboard once (Enter accepts), unless the person was typing.
    let take_keys =
        current.as_ref().is_some_and(|r| r.review().is_some()) && app.ask_agent.bubble.as_ref().is_some_and(|b| b.reviewed != b.request && !b.typed);
    if let Some(b) = app.ask_agent.bubble.as_mut().filter(|_| current.as_ref().is_some_and(|r| r.review().is_some())) {
        b.reviewed = b.request;
    }
    let mut keys_here = false;
    let height = app.ask_agent.bubble.as_ref().map_or(360.0, |b| b.size.y);
    let pos = bubble_pos(at, vec2(width, height), viewport)?;
    let chars = app.session.selected_text().chars().count();
    let present = hub.as_ref().is_some_and(AgentHub::agent_present);
    let listening = hub.as_ref().and_then(AgentHub::agent_name);
    let animate = app.animations_on();
    let edit_id = egui::Id::new("wc_ask_agent_text");
    let typing = ctx.memory(|m| m.has_focus(edit_id));
    let mut act = BubbleAct::None;
    let t = Tokens::get(ctx);
    let shown = egui::Area::new(egui::Id::new("wc_ask_agent")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
        card(&t).show(ui, |ui| {
            ui.set_width(width);
            if header(ui, &t) {
                act = BubbleAct::Close;
            }
            match &current {
                None => {
                    let about = if chars == 0 {
                        tl!("No text is selected: the agent gets the caret's place.").to_string()
                    } else {
                        crate::i18n::fmt(tl!("The agent gets the selected text ({count} characters)."), &[("count", &chars.to_string())])
                    };
                    ui.label(egui::RichText::new(about).small().weak());
                    // Enter sends, Shift+Enter starts a new line, Up in an empty box brings back the
                    // last instruction.
                    let enter = typing && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                    let history = app.ask_agent.history.clone();
                    let empty = app.ask_agent.bubble.as_ref().is_some_and(|b| b.text.is_empty());
                    let up = typing && empty && !history.is_empty() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp));
                    let Some(b) = app.ask_agent.bubble.as_mut() else { return };
                    if let (true, Some(last)) = (up, history.first()) {
                        b.text = last.clone();
                    }
                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut b.text)
                            .id(edit_id)
                            .hint_text(tl!("Ask agent to…"))
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    );
                    if !b.focused {
                        edit.request_focus();
                        b.focused = true;
                    }
                    if up {
                        // The caret at the end of the recalled text.
                        let end = b.text.chars().count();
                        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit_id) {
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
                            state.store(ui.ctx(), edit_id);
                        }
                    }
                    ui.label(egui::RichText::new(tl!("Enter to send, Shift+Enter for a new line")).small().color(t.text_dim));
                    let has_text = !b.text.trim().is_empty();
                    if !has_text && !history.is_empty() {
                        ui.label(egui::RichText::new(tl!("Recent")).small().weak());
                        for h in history.iter().take(HISTORY_SHOWN) {
                            // A recent instruction fills the box, to send as it is or change first.
                            if ui.add(egui::Label::new(h.as_str()).truncate().sense(egui::Sense::click())).on_hover_text(h.as_str()).clicked() {
                                b.text = h.clone();
                                edit.request_focus();
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(has_text, egui::Button::new(tl!("Send"))).clicked() || (enter && has_text) {
                            act = BubbleAct::Send;
                        }
                        if ui.button(tl!("Cancel")).clicked() {
                            act = BubbleAct::Close;
                        }
                    });
                    ui.separator();
                    if let Some(id) = quick_actions(ui) {
                        act = BubbleAct::Action(id);
                    }
                }
                Some(r) => {
                    // What was asked, in grey, then where it stands.
                    ui.add(egui::Label::new(egui::RichText::new(shown_title(r)).small().color(t.text_dim)).truncate()).on_hover_text(&r.instruction);
                    status(ui, r, present, animate, listening.as_deref());
                    if let Some(p) = r.proposal.as_ref().filter(|p| !p.text.is_empty()) {
                        preview(ui, &t, r, p);
                    }
                    let review = r.review();
                    if review == Some(Review::Edits) {
                        ui.label(
                            egui::RichText::new(tl!("The agent's changes are tracked: Accept keeps them, Discard rejects them.")).small().weak(),
                        );
                    }
                    ui.horizontal(|ui| {
                        if r.state.open() {
                            if ui.button(tl!("Cancel")).clicked() {
                                act = BubbleAct::Cancel(r.id);
                            }
                        } else if let Some(review) = review {
                            let (a, d, re) = review_buttons(ui, review, &r.author(), take_keys);
                            keys_here = a.has_focus() || d.has_focus() || re.has_focus();
                            if a.clicked() {
                                act = BubbleAct::Accept(r.id);
                            } else if d.clicked() {
                                act = BubbleAct::Discard(r.id);
                            } else if re.clicked() {
                                act = BubbleAct::Retry(r.id);
                            }
                        } else {
                            if r.state == State::Answered && ui.button(tl!("Retry")).on_hover_text(retry_hint()).clicked() {
                                act = BubbleAct::Retry(r.id);
                            }
                            if ui.button(tl!("Close")).clicked() {
                                act = BubbleAct::Dismiss(r.id);
                            }
                        }
                    });
                }
            }
        });
    });
    // Escape, or a press in the document outside the bubble, closes it (a request it sent goes on
    // in the corner card; an answered one is put away).
    let rect = shown.response.rect;
    if let Some(b) = app.ask_agent.bubble.as_mut() {
        b.size = rect.size();
    }
    let escape = ctx.input(|i| i.key_pressed(Key::Escape));
    let press = ctx.input(|i| if i.pointer.any_pressed() { i.pointer.press_origin() } else { None });
    let elsewhere =
        press.is_some_and(|p| viewport.contains(p) && !rect.contains(p) && !ctx.layer_id_at(p).is_some_and(|l| l.order >= Order::Foreground));
    // With the bubble's buttons holding the keyboard, Ctrl+R (⌘R) asks again.
    let retry_key = keys_here && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::R));
    if let (BubbleAct::None, true, Some(r)) = (&act, retry_key, &current) {
        act = BubbleAct::Retry(r.id);
    }
    let held = app.ask_agent.bubble.as_mut().is_some_and(|b| std::mem::replace(&mut b.held_keys, keys_here));
    if matches!(act, BubbleAct::None) && (escape || elsewhere) {
        act = match &current {
            // Escape discards a result waiting for review while the bubble has the keyboard;
            // otherwise (the person was typing in the document), and after a click elsewhere, the
            // result waits in the corner card.
            Some(r) if r.review().is_some() && escape && (held || keys_here) => BubbleAct::Discard(r.id),
            Some(r) if r.review().is_some() => BubbleAct::Close,
            Some(r) if !r.state.open() => BubbleAct::Dismiss(r.id),
            _ => BubbleAct::Close,
        };
    }
    match act {
        BubbleAct::None => {}
        BubbleAct::Send => send(app),
        BubbleAct::Action(id) => send_action(app, &id),
        BubbleAct::Close => {
            app.ask_agent.bubble = None;
            app.canvas.want_focus = true;
        }
        BubbleAct::Cancel(id) => {
            if let Some(h) = &hub {
                h.cancel(id);
            }
        }
        BubbleAct::Dismiss(id) => {
            if let Some(h) = &hub {
                h.dismiss(id);
            }
            app.ask_agent.bubble = None;
            app.canvas.want_focus = true;
        }
        BubbleAct::Accept(id) => review_act(app, "accept", id),
        BubbleAct::Discard(id) => review_act(app, "discard", id),
        BubbleAct::Retry(id) => review_act(app, "retry", id),
    }
    request
}

enum BubbleAct {
    None,
    Send,
    Action(String),
    Close,
    Cancel(u64),
    Dismiss(u64),
    Accept(u64),
    Discard(u64),
    Retry(u64),
}

/// Accept, Discard or Retry from a button or a key; a refusal goes to the status bar.
fn review_act(app: &mut WordApp, what: &str, id: u64) {
    if let Err(e) = command(app, &json!({what: id})) {
        app.status(tl!(&e).to_string());
    }
}

/// Retry's tooltip.
fn retry_hint() -> String {
    format!("{} ({})", tl!("Ask again: the same request, a new answer"), crate::widgets::key_text("Mod+R"))
}

/// Accept (the accent button), Discard and Retry for a result, with what each does and its key.
/// `focus` gives Accept the keyboard (Enter accepts, Tab goes to the others).
fn review_buttons(ui: &mut egui::Ui, review: Review, author: &str, focus: bool) -> (egui::Response, egui::Response, egui::Response) {
    let (accept_tip, discard_tip) = match review {
        Review::Proposal => (
            crate::i18n::fmt(tl!("Put this text in the document, as a tracked change by {author}"), &[("author", author)]),
            tl!("Drop this proposal; the document doesn't change").to_string(),
        ),
        Review::Edits => (tl!("Make the agent's tracked changes final").to_string(), tl!("Reject the agent's tracked changes").to_string()),
    };
    let accept = ui.add(crate::widgets::primary_button(ui, tl!("Accept"))).on_hover_text(format!("{accept_tip} ({})", tl!("Enter")));
    if focus {
        accept.request_focus();
    }
    let discard = ui.button(tl!("Discard")).on_hover_text(format!("{discard_tip} ({})", tl!("Esc")));
    let retry = ui.button(tl!("Retry")).on_hover_text(retry_hint());
    (accept, discard, retry)
}

/// The proposal in the bubble: the text as it streams in (scrolled to its end while the agent
/// writes); once done, for a plain-text proposal that replaces the selection, the words it removes
/// struck through and the words it adds underlined.
fn preview(ui: &mut egui::Ui, t: &Tokens, r: &Request, p: &Proposal) {
    let original = r.selection.get("text").and_then(Value::as_str).unwrap_or("");
    let whole = r.selection.get("truncated").and_then(Value::as_bool) != Some(true);
    let compare = r.state == State::Answered && !p.markdown && r.apply != Apply::InsertBelow && !original.is_empty() && whole;
    egui::Frame::new()
        .fill(t.input)
        .stroke(egui::Stroke::new(1.0, t.input_border))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("wc_ask_agent_proposal", r.id))
                .max_height(220.0)
                .auto_shrink([false, true])
                .stick_to_bottom(r.state.open())
                .show(ui, |ui| {
                    // Computed once per proposal (it no longer changes once done), not every frame.
                    let key = egui::Id::new(("wc_ask_agent_diff", r.id, p.text.len()));
                    let diff = if compare {
                        ui.ctx().data(|d| d.get_temp::<Option<Arc<Vec<(Change, String)>>>>(key)).unwrap_or_else(|| {
                            let parts = word_diff(original, &p.text).map(Arc::new);
                            ui.ctx().data_mut(|d| d.insert_temp(key, parts.clone()));
                            parts
                        })
                    } else {
                        None
                    };
                    match diff {
                        Some(parts) => {
                            let font = egui::TextStyle::Body.resolve(ui.style());
                            let mut job = egui::text::LayoutJob::default();
                            for (change, text) in parts.iter() {
                                let mut f = egui::TextFormat { font_id: font.clone(), color: t.text, ..Default::default() };
                                match change {
                                    Change::Same => {}
                                    Change::Removed => {
                                        f.color = t.red;
                                        f.strikethrough = egui::Stroke::new(1.0, t.red);
                                    }
                                    Change::Added => {
                                        f.color = t.green;
                                        f.underline = egui::Stroke::new(1.0, t.green);
                                    }
                                }
                                job.append(text, 0.0, f);
                            }
                            job.wrap.max_width = ui.available_width();
                            ui.add(egui::Label::new(job).selectable(true).wrap());
                        }
                        None => drop(ui.add(egui::Label::new(p.text.as_str()).selectable(true).wrap())),
                    }
                });
        });
}

/// A piece of a word comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Same,
    Removed,
    Added,
}

/// Comparisons bigger than this (words before × words after) aren't made.
const MAX_DIFF_CELLS: usize = 250_000;

/// `old` → `new` word by word (words, spaces and punctuation are the pieces), the longest common
/// run kept: what stays, what goes, what comes, in order, neighbours of a kind joined. `None` when
/// the texts are too long to compare.
pub fn word_diff(old: &str, new: &str) -> Option<Vec<(Change, String)>> {
    let (a, b) = (pieces(old), pieces(new));
    let (n, m) = (a.len(), b.len());
    if n.checked_mul(m).is_none_or(|cells| cells > MAX_DIFF_CELLS) {
        return None;
    }
    // lcs[i][j]: the longest common run of a[i..] and b[j..].
    let w = m + 1;
    let mut lcs = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let v = if a.get(i) == b.get(j) {
                lcs.get((i + 1) * w + j + 1).copied().unwrap_or(0).saturating_add(1)
            } else {
                lcs.get((i + 1) * w + j).copied().unwrap_or(0).max(lcs.get(i * w + j + 1).copied().unwrap_or(0))
            };
            if let Some(c) = lcs.get_mut(i * w + j) {
                *c = v;
            }
        }
    }
    let at = |i: usize, j: usize| lcs.get(i * w + j).copied().unwrap_or(0);
    let mut out: Vec<(Change, String)> = Vec::new();
    let mut push = |c: Change, s: &str| match out.last_mut() {
        Some((last, text)) if *last == c => text.push_str(s),
        _ => out.push((c, s.to_string())),
    };
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x == y => {
                push(Change::Same, x);
                i += 1;
                j += 1;
            }
            (Some(x), Some(_)) if at(i + 1, j) >= at(i, j + 1) => {
                push(Change::Removed, x);
                i += 1;
            }
            (_, Some(y)) => {
                push(Change::Added, y);
                j += 1;
            }
            (Some(x), None) => {
                push(Change::Removed, x);
                i += 1;
            }
            (None, None) => break,
        }
    }
    Some(out)
}

/// A text cut into words (letters and digits), runs of spaces, and single other characters.
fn pieces(text: &str) -> Vec<&str> {
    let kind = |c: char| {
        if c.is_alphanumeric() {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<u8> = None;
    for (i, c) in text.char_indices() {
        let k = kind(c);
        if let Some(p) = prev
            && (p != k || k == 2)
        {
            if let Some(piece) = text.get(start..i) {
                out.push(piece);
            }
            start = i;
        }
        prev = Some(k);
    }
    if let Some(piece) = text.get(start..).filter(|p| !p.is_empty()) {
        out.push(piece);
    }
    out
}

/// The quick actions, as a menu: top-level ones as items, the others in their submenus (Rewrite,
/// Translate, More), in [`crate::agent_actions::defaults`]' order. The id of the one clicked.
fn quick_actions(ui: &mut egui::Ui) -> Option<String> {
    let all = crate::agent_actions::defaults();
    let mut picked = None;
    let mut shown: Vec<&str> = Vec::new();
    let hint = |a: &AgentAction| match a.apply {
        Apply::InsertBelow => tl!("Inserts the result after the selection"),
        Apply::ReplyOnly => tl!("Answers in the bubble; changes nothing"),
        Apply::Replace | Apply::Auto => tl!("Replaces the selection; the change is tracked"),
    };
    ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
        for a in &all {
            match a.menu.as_deref() {
                None => {
                    if ui.add(egui::Button::new(tl!(&a.label)).frame(false)).on_hover_text(hint(a)).clicked() {
                        picked = Some(a.id.clone());
                    }
                }
                Some(menu) if !shown.contains(&menu) && MENUS.contains(&menu) => {
                    shown.push(menu);
                    ui.menu_button(tl!(menu), |ui| {
                        for b in all.iter().filter(|b| b.menu.as_deref() == Some(menu)) {
                            if ui.button(tl!(&b.label)).on_hover_text(hint(b)).clicked() {
                                picked = Some(b.id.clone());
                                ui.close();
                            }
                        }
                    });
                }
                Some(_) => {}
            }
        }
    });
    picked
}

/// A request as the window names it: the person's words, or the action's label translated.
fn shown_title(r: &Request) -> String {
    match &r.action {
        Some(a) => tl!(&a.label).to_string(),
        None => r.instruction.clone(),
    }
}

/// A request's state, live: "Sent to your agent" the moment it is sent, "Your agent is working"
/// as soon as the agent has it, with a spinner (a still line with animations off) and the time
/// since it was sent; the agent's latest progress note; then its answer, whole (it wraps, and
/// scrolls when long), or Cancelled.
fn status(ui: &mut egui::Ui, r: &Request, present: bool, animate: bool, listening: Option<&str>) {
    let t = Tokens::get(ui.ctx());
    let secs = ((crate::now_ms() - r.asked_ms) / 1000.0).max(0.0) as u64;
    let busy = |ui: &mut egui::Ui, text: &str| {
        ui.horizontal(|ui| {
            if animate {
                ui.add(egui::Spinner::new().size(12.0).color(t.accent));
            } else {
                // The seconds still count.
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            }
            ui.label(text);
            ui.label(egui::RichText::new(crate::i18n::fmt(tl!("{seconds} s"), &[("seconds", &secs.to_string())])).small().color(t.text_dim));
        });
    };
    match r.state {
        State::Waiting if present => match listening {
            Some(agent) => busy(ui, &crate::i18n::fmt(tl!("Sent to {agent}…"), &[("agent", agent)])),
            None => busy(ui, tl!("Sent to your agent…")),
        },
        State::Waiting => {
            busy(ui, tl!("Waiting for an agent…"));
            ui.label(egui::RichText::new(tl!("No agent is listening right now.")).small().weak());
        }
        State::Working => {
            match &r.agent {
                Some(agent) => busy(ui, &crate::i18n::fmt(tl!("{agent} is working…"), &[("agent", agent)])),
                None => busy(ui, tl!("Your agent is working…")),
            }
            // Its latest progress note.
            if !r.reply.is_empty() {
                ui.add(egui::Label::new(r.reply.as_str()).wrap());
            }
        }
        // The agent's own words: shown whole, as it wrote them, selectable to copy.
        // A proposal says enough by itself (it shows below).
        State::Answered if r.reply.is_empty() && r.proposal.is_some() => {}
        State::Answered if r.reply.is_empty() => drop(ui.label(tl!("Done."))),
        State::Answered => {
            egui::ScrollArea::vertical().id_salt(("wc_ask_agent_reply", r.id)).max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.add(egui::Label::new(r.reply.as_str()).selectable(true).wrap());
            });
        }
        State::Cancelled => drop(ui.label(egui::RichText::new(tl!("Cancelled")).weak())),
    }
}

/// The bubble's and the card's look: the mini toolbar's (menu fill, strong border, 6 pt corners,
/// a soft shadow), with room inside.
fn card(t: &Tokens) -> egui::Frame {
    egui::Frame::new()
        .fill(t.menu)
        .stroke(egui::Stroke::new(1.0, t.border_strong))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(10))
        .shadow(egui::Shadow { offset: [0, 2], blur: 8, spread: 0, color: t.page_shadow })
}

/// The bubble's title row: the Ask Agent icon and name, and a close button. True when the close
/// button was clicked.
fn header(ui: &mut egui::Ui, t: &Tokens) -> bool {
    ui.horizontal(|ui| {
        let (icon, _) = ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::hover());
        crate::icons::paint(ui.painter(), icon, "askAgent", t.icon, t.accent);
        ui.label(egui::RichText::new(tl!("Ask Agent")).font(crate::theme::semibold(12.5)).color(t.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| close_button(ui, t)).inner
    })
    .inner
}

/// A small × like the panes' close buttons: a hover square and two strokes.
fn close_button(ui: &mut egui::Ui, t: &Tokens) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, egui::CornerRadius::same(4), t.hover);
    }
    let (c, d) = (r.center(), 3.5);
    let stroke = egui::Stroke::new(1.3, t.icon);
    ui.painter().line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke);
    ui.painter().line_segment([c + vec2(-d, d), c + vec2(d, -d)], stroke);
    let label = tl!("Close");
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    resp.on_hover_text(label).clicked()
}

/// The requests the bubble doesn't show, newest last, in the canvas's bottom-right corner.
fn show_requests(app: &mut WordApp, ctx: &egui::Context, viewport: Rect, in_bubble: Option<u64>) {
    let Some(hub) = app.ask_agent.hub.clone() else { return };
    let requests: Vec<Request> = hub.requests().into_iter().filter(|r| Some(r.id) != in_bubble).collect();
    if requests.is_empty() {
        return;
    }
    let present = hub.agent_present();
    let listening = hub.agent_name();
    let animate = app.animations_on();
    let t = Tokens::get(ctx);
    let mut act = None;
    egui::Area::new(egui::Id::new("wc_ask_agent_requests"))
        .order(Order::Foreground)
        .pivot(Align2::RIGHT_BOTTOM)
        .fixed_pos(viewport.right_bottom() - vec2(16.0, 16.0))
        .show(ctx, |ui| {
            card(&t).show(ui, |ui| {
                ui.set_width(WIDTH);
                let mut close = false;
                if header(ui, &t) {
                    close = true;
                }
                for r in requests.iter().rev().take(SHOWN).rev() {
                    ui.separator();
                    ui.add(egui::Label::new(egui::RichText::new(shown_title(r)).small().color(t.text_dim)).truncate());
                    status(ui, r, present, animate, listening.as_deref());
                    if let Some(p) = r.proposal.as_ref().filter(|p| !p.text.is_empty()) {
                        // The proposal's start; the whole of it is in the bubble.
                        ui.add(egui::Label::new(egui::RichText::new(p.text.as_str()).small()).truncate()).on_hover_text(p.text.as_str());
                    }
                    if r.state.open() {
                        if ui.small_button(tl!("Cancel")).clicked() {
                            hub.cancel(r.id);
                        }
                    } else if r.review().is_some() {
                        ui.horizontal(|ui| {
                            if ui.small_button(tl!("Accept")).clicked() {
                                act = Some(("accept", r.id));
                            }
                            if ui.small_button(tl!("Discard")).clicked() {
                                act = Some(("discard", r.id));
                            }
                            if ui.small_button(tl!("Retry")).on_hover_text(retry_hint()).clicked() {
                                act = Some(("retry", r.id));
                            }
                        });
                    } else if ui.small_button(tl!("Dismiss")).clicked() {
                        hub.dismiss(r.id);
                    }
                }
                // Closing the card puts away the answered and cancelled requests; open ones stay.
                if close {
                    for r in &requests {
                        hub.dismiss(r.id);
                    }
                }
            });
        });
    if let Some((what, id)) = act {
        review_act(app, what, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sel(text: &str) -> Value {
        json!({"text": text})
    }

    #[test]
    fn requests_go_out_oldest_first_and_once() {
        let hub = AgentHub::new(7981, None);
        assert!(hub.listen(Duration::from_millis(10), || true).is_none(), "nothing queued");
        let a = hub.ask("  shorten this  ", sel("Kilns fire clay."), json!({"title": null})).unwrap();
        let b = hub.ask("translate", sel(""), json!({"title": null})).unwrap();
        assert!(b > a);
        let got = hub.listen(Duration::from_millis(10), || true).unwrap();
        assert_eq!((got.id, got.instruction.as_str(), got.state), (a, "shorten this", State::Working));
        assert_eq!(got.to_json()["selection"]["text"], "Kilns fire clay.");
        assert_eq!(hub.listen(Duration::from_millis(10), || true).unwrap().id, b);
        assert!(hub.listen(Duration::from_millis(10), || true).is_none(), "each request goes to one agent");
    }

    #[test]
    fn listen_waits_on_its_own_thread_until_a_request_arrives() {
        let hub = AgentHub::new(7981, None);
        let agent = hub.clone();
        let waiting = std::thread::spawn(move || agent.listen(Duration::from_secs(10), || true));
        // The listener counts as an agent while it waits.
        let t0 = std::time::Instant::now();
        while !hub.agent_present() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(hub.agent_present());
        std::thread::sleep(Duration::from_millis(50));
        let id = hub.ask("make it formal", sel("hey"), json!({})).unwrap();
        let got = waiting.join().unwrap().expect("woken by the request");
        assert_eq!(got.id, id);
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_client_that_has_gone_gets_nothing_and_the_request_stays() {
        let hub = AgentHub::new(7981, None);
        let id = hub.ask("fix the typos", sel("teh"), json!({})).unwrap();
        assert!(hub.listen(Duration::from_millis(10), || false).is_none());
        assert_eq!(hub.listen(Duration::from_millis(10), || true).unwrap().id, id);
        // A reply that never reached the agent: back in the queue.
        hub.requeue(id);
        assert_eq!(hub.listen(Duration::from_millis(10), || true).unwrap().id, id);
    }

    #[test]
    fn replies_show_and_close_cancelled_requests_say_so() {
        let hub = AgentHub::new(7981, None);
        let id = hub.ask("shorten", sel("a b c"), json!({})).unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        assert_eq!(hub.reply(id, "Working on it", false).unwrap()["done"], false);
        assert_eq!(hub.requests()[0].state, State::Working);
        assert_eq!(hub.reply(id, "Shortened to one sentence.", true).unwrap()["shown"], true);
        assert_eq!((hub.requests()[0].state, hub.requests()[0].reply.as_str()), (State::Answered, "Shortened to one sentence."));
        assert!(hub.reply(id, "again", true).unwrap_err().contains("already answered"));
        assert!(hub.reply(999, "?", true).unwrap_err().contains("no Ask Agent request 999"));
        // Cancelled while the agent works: its reply learns it.
        let id = hub.ask("translate", sel("x"), json!({})).unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        assert!(hub.cancel(id));
        assert!(!hub.cancel(id), "only once");
        assert_eq!(hub.reply(id, "Done", true).unwrap()["cancelled"], true);
        // A cancelled request is never handed out.
        let id = hub.ask("later", sel(""), json!({})).unwrap();
        hub.cancel(id);
        assert!(hub.listen(Duration::from_millis(10), || true).is_none());
        hub.dismiss(id);
        assert!(hub.requests().iter().all(|r| r.id != id));
    }

    #[test]
    fn hostile_text_is_capped_and_empty_instructions_refused() {
        let hub = AgentHub::new(7981, None);
        assert!(hub.ask("   ", sel(""), json!({})).is_err());
        let id = hub.ask(&"é".repeat(MAX_TEXT_CHARS + 50), sel(""), json!({})).unwrap();
        let r = &hub.requests()[0];
        assert_eq!(r.instruction.chars().count(), MAX_TEXT_CHARS + 1, "cut, with …");
        hub.reply(id, &"ü".repeat(MAX_TEXT_CHARS * 3), true).unwrap();
        assert_eq!(hub.requests()[0].reply.chars().count(), MAX_TEXT_CHARS + 1);
        let mut s = "ab€".repeat(10);
        assert!(cap_bytes(&mut s, 4));
        assert_eq!(s, "ab", "cut at a character boundary");
        // The queue stays bounded however many requests come.
        for i in 0..200 {
            let id = hub.ask(&format!("r{i}"), sel(""), json!({})).unwrap();
            hub.cancel(id);
        }
        assert!(hub.requests().len() <= KEEP);
        for i in 0..200 {
            hub.ask(&format!("open {i}"), sel(""), json!({})).unwrap();
        }
        assert!(hub.requests().len() <= 2 * KEEP);
    }

    #[test]
    fn presence_follows_the_agent() {
        let hub = AgentHub::new(7985, None);
        assert!(!hub.agent_present(), "no agent has listened");
        let app = AskAgent { hub: Some(hub.clone()), ..Default::default() };
        assert_eq!(app.unavailable(), Some(Unavailable::NoAgent(7985)));
        assert!(app.unavailable().unwrap().english().contains("wordcraft-cli mcp --connect 127.0.0.1:7985"));
        hub.listen(Duration::from_millis(1), || true);
        assert!(hub.agent_present(), "listened a moment ago");
        assert_eq!(app.unavailable(), None);
        assert_eq!(AskAgent::default().unavailable(), Some(Unavailable::NoControlChannel));
    }

    #[test]
    fn recent_instructions_are_few_unique_and_newest_first() {
        let mut h = Vec::new();
        for i in 0..20 {
            remember(&mut h, &format!("  step {i} "));
        }
        remember(&mut h, "step 15");
        remember(&mut h, "   ");
        assert_eq!(h.len(), HISTORY);
        assert_eq!(h[0], "step 15");
        assert_eq!(h.iter().filter(|x| *x == "step 15").count(), 1);
    }

    #[test]
    fn the_bubble_and_chip_stay_inside_the_canvas() {
        let view = Rect::from_min_size(pos2(0.0, 100.0), vec2(800.0, 600.0));
        let size = vec2(WIDTH, 150.0);
        // Below the line.
        let p = bubble_pos(Some((pos2(100.0, 200.0), 20.0)), size, view).unwrap();
        assert!(p.y >= 220.0 && p.x == 100.0, "{p:?}");
        // Near the bottom: above the line instead; near the right edge: pulled in.
        let p = bubble_pos(Some((pos2(790.0, 680.0), 20.0)), size, view).unwrap();
        assert!(p.y + size.y <= 680.0 && p.x + size.x <= 800.0, "{p:?}");
        // No anchor on screen, or junk: the canvas's corner, never NaN.
        assert!(bubble_pos(None, size, view).is_some_and(|p| view.contains(p)));
        assert!(bubble_pos(Some((pos2(f32::NAN, 1.0), 1.0)), size, view).is_some_and(|p| p.is_finite()));
        assert!(bubble_pos(None, size, Rect::NOTHING).is_none());
        // The chip floats beside the mini toolbar, on the side that fits, else under it.
        let chip = vec2(100.0, CHIP_H);
        let line = Rect::from_min_size(pos2(100.0, 300.0), vec2(200.0, 18.0));
        let bar = Rect::from_min_size(pos2(80.0, 226.0), vec2(300.0, 60.0));
        assert_eq!(chip_pos(line, None, chip, view, Some(bar)), Some(pos2(386.0, 226.0)));
        let right_edge = Rect::from_min_size(pos2(500.0, 226.0), vec2(296.0, 60.0));
        assert_eq!(chip_pos(line, None, chip, view, Some(right_edge)), Some(pos2(394.0, 226.0)), "left of the bar");
        let wide = Rect::from_min_size(pos2(0.0, 226.0), vec2(800.0, 60.0));
        let p = chip_pos(line, None, chip, view, Some(wide)).unwrap();
        assert!(p.y >= wide.bottom() && view.contains_rect(Rect::from_min_size(p, chip)), "{p:?}");
        // No bar: where the bar would be, above the first line; under the end when that is the
        // canvas's top line.
        assert_eq!(chip_pos(line, None, chip, view, None), Some(pos2(100.0, 300.0 - GAP - CHIP_H)));
        let top = Rect::from_min_size(pos2(100.0, 105.0), vec2(200.0, 18.0));
        let p = chip_pos(top, Some((pos2(300.0, 105.0), 18.0)), chip, view, None).unwrap();
        assert!(p.y >= 123.0, "{p:?}");
        assert!(chip_pos(Rect::NAN, None, chip, view, None).is_none());
    }

    // The whole window, headless.

    fn window(hub: Option<AgentHub>) -> egui_kittest::Harness<'static, WordApp> {
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), Default::default());
        if let Some(hub) = hub {
            a = a.with_agent_hub(hub);
        }
        a.run("text.insert", json!({"text": "Kilns fire the clay slowly over many hours."})).unwrap();
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1440.0, 900.0)).build_ui_state(
            |ui, app: &mut WordApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            a,
        );
        steps(&mut h);
        h
    }

    fn steps(h: &mut egui_kittest::Harness<'static, WordApp>) {
        for _ in 0..6 {
            h.step();
        }
    }

    fn chip(h: &egui_kittest::Harness<'static, WordApp>) -> Option<Rect> {
        let shown = h.ctx.memory(|m| m.areas().visible_layer_ids().into_iter().any(|l| l.id == egui::Id::new("wc_ask_agent_chip")));
        shown.then(|| h.ctx.data(|d| d.get_temp::<Rect>(chip_id()))).flatten()
    }

    fn click(h: &mut egui_kittest::Harness<'static, WordApp>, at: Pos2) {
        h.event(egui::Event::PointerMoved(at));
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
        }
        steps(h);
    }

    fn key(h: &mut egui_kittest::Harness<'static, WordApp>, key: Key, modifiers: Modifiers) {
        h.event(egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
        steps(h);
    }

    /// The chip shows on a selection, only while an agent listens, and goes with the selection.
    #[test]
    fn the_chip_shows_on_a_selection_while_an_agent_listens() {
        // No control channel: nothing floats.
        let mut h = window(None);
        h.state_mut().run("select.text", json!({"text": "the clay"})).unwrap();
        steps(&mut h);
        assert!(chip(&h).is_none(), "no control channel, no chip");
        // A control channel, no agent yet: still nothing.
        let hub = AgentHub::new(7985, None);
        let mut h = window(Some(hub.clone()));
        h.state_mut().run("select.text", json!({"text": "the clay"})).unwrap();
        steps(&mut h);
        assert!(chip(&h).is_none(), "no agent listening, no chip");
        // An agent listens: the chip is on the selection, below its end.
        hub.listen(Duration::from_millis(1), || true);
        steps(&mut h);
        let at = chip(&h).expect("the chip shows while an agent listens");
        let canvas = h.state().canvas.canvas_rect.expect("canvas");
        assert!(canvas.contains_rect(at), "{at:?} inside {canvas:?}");
        // Escape collapses the selection: the chip goes. So does a caret.
        h.state_mut().canvas.want_focus = true;
        steps(&mut h);
        key(&mut h, Key::Escape, Modifiers::NONE);
        assert!(h.state().session.sel.is_collapsed());
        assert!(chip(&h).is_none(), "no chip on a caret");
    }

    /// Chip → bubble → instruction → the agent gets it with the selection, its answer shows.
    #[test]
    fn the_chip_opens_the_bubble_and_the_request_reaches_the_agent() {
        use egui_kittest::kittest::Queryable;
        let hub = AgentHub::new(7985, None);
        hub.listen(Duration::from_millis(1), || true);
        let mut h = window(Some(hub.clone()));
        h.state_mut().run("select.text", json!({"text": "the clay slowly"})).unwrap();
        steps(&mut h);
        let at = chip(&h).expect("chip").center();
        click(&mut h, at);
        assert!(h.state().ask_agent.bubble.is_some(), "the chip opened the bubble");
        assert!(chip(&h).is_none(), "the chip gives way to the bubble");
        assert!(h.query_by_label_contains("15 characters").is_some(), "the bubble says what the agent gets");
        h.event(egui::Event::Text("make it formal".into()));
        steps(&mut h);
        key(&mut h, Key::Enter, Modifiers::NONE);
        // Sent: the bubble says so at once, before any agent has it.
        assert!(h.query_by_label_contains("Sent to your agent").is_some(), "immediate feedback");
        let r = hub.listen(Duration::from_millis(10), || true).expect("the request is queued");
        assert_eq!(r.instruction, "make it formal");
        assert_eq!(r.selection["text"], "the clay slowly");
        assert_eq!(r.document, json!({"title": "Document1", "saved": false, "language": "en-US"}), "a new document goes by the window's name");
        assert_eq!(h.state().ask_agent.history, vec!["make it formal".to_string()]);
        steps(&mut h);
        assert!(h.query_by_label_contains("Your agent is working").is_some(), "the bubble says the agent has it");
        // A progress note shows while the agent works.
        hub.reply(r.id, "Rewriting…", false).unwrap();
        steps(&mut h);
        assert!(h.query_by_label_contains("Rewriting…").is_some(), "the latest progress note");
        hub.reply(r.id, "Rewrote it in a formal register.", true).unwrap();
        steps(&mut h);
        assert!(h.query_by_label_contains("Rewrote it in a formal register.").is_some(), "the answer shows in the bubble");
        // The document itself is the agent's to change (through the usual commands); nothing here.
        assert_eq!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body), "Kilns fire the clay slowly over many hours.");
        // Escape puts the answered request away.
        key(&mut h, Key::Escape, Modifiers::NONE);
        assert!(h.state().ask_agent.bubble.is_none());
        assert!(hub.requests().is_empty());
    }

    /// Cancelling from the bubble reaches the agent; the shortcut opens the bubble too, Escape
    /// closes it without sending anything, and with no agent the shortcut says why in the status
    /// bar.
    #[test]
    fn cancel_shortcut_and_no_agent() {
        use egui_kittest::kittest::Queryable;
        let hub = AgentHub::new(7985, None);
        let mut h = window(Some(hub.clone()));
        h.state_mut().run("select.text", json!({"text": "Kilns"})).unwrap();
        h.state_mut().canvas.want_focus = true;
        steps(&mut h);
        key(&mut h, Key::O, Modifiers::COMMAND | Modifiers::SHIFT);
        assert!(h.state().ask_agent.bubble.is_none(), "no agent: no bubble");
        let msg = h.state().status_msg.clone().map(|m| m.0).unwrap_or_default();
        assert!(msg.contains("No agent is listening") && msg.contains("127.0.0.1:7985"), "{msg}");
        hub.listen(Duration::from_millis(1), || true);
        h.state_mut().canvas.want_focus = true;
        steps(&mut h);
        key(&mut h, Key::O, Modifiers::COMMAND | Modifiers::SHIFT);
        assert!(h.state().ask_agent.bubble.is_some(), "Ctrl+Shift+O opened the bubble");
        key(&mut h, Key::Escape, Modifiers::NONE);
        assert!(h.state().ask_agent.bubble.is_none() && hub.requests().is_empty(), "Escape sends nothing");
        // Send, then cancel from the bubble.
        h.state_mut().run("ui.askAgent", json!({})).unwrap();
        steps(&mut h);
        h.event(egui::Event::Text("translate into French".into()));
        steps(&mut h);
        key(&mut h, Key::Enter, Modifiers::NONE);
        let r = hub.listen(Duration::from_millis(10), || true).expect("queued");
        let cancel = h.get_by_label("Cancel").rect().center();
        click(&mut h, cancel);
        assert_eq!(hub.reply(r.id, "Traduit.", true).unwrap()["cancelled"], true, "the agent learns it was cancelled");
    }

    /// Without a control channel `ui.askAgent` says how to get one; programmatic requests and
    /// `ui.inspect` work for scripts.
    #[test]
    fn scripts_and_no_control_channel() {
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), Default::default());
        let e = a.run("ui.askAgent", json!({})).unwrap_err();
        assert!(e.contains("--control"), "{e}");
        let hub = AgentHub::new(7981, None);
        let mut a = a.with_agent_hub(hub.clone());
        a.run("text.insert", json!({"text": "Glaze"})).unwrap();
        a.run("select.all", json!({})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "explain glaze"})).unwrap()["id"].as_u64().unwrap();
        assert_eq!(hub.requests()[0].selection["text"], "Glaze");
        let ctx = egui::Context::default();
        let v = crate::control::inspect(&mut a, &ctx);
        assert_eq!(v["askAgent"]["requests"][0]["state"], "waiting");
        assert_eq!(v["askAgent"]["agentListening"], false);
        assert!(v["askAgent"]["reason"].as_str().unwrap().contains("No agent is listening"));
        assert_eq!(a.run("ui.askAgent", json!({"cancel": id})).unwrap()["cancelled"], true);
        assert!(a.run("ui.askAgent", json!({"instruction": 5})).is_err());
        assert!(a.run("ui.askAgent", json!({"cancel": "x"})).is_err());
    }

    /// Keyboard: Up in the empty box brings back the last instruction; the chip shows in the very
    /// frame the selection is made.
    #[test]
    fn up_recalls_the_last_instruction_and_the_chip_comes_at_once() {
        let hub = AgentHub::new(7985, None);
        hub.listen(Duration::from_millis(1), || true);
        let mut h = window(Some(hub.clone()));
        h.state_mut().ask_agent.history = vec!["make it formal".into(), "older".into()];
        h.state_mut().run("select.text", json!({"text": "Kilns"})).unwrap();
        h.step();
        assert!(chip(&h).is_some(), "one frame after the selection");
        h.state_mut().run("ui.askAgent", json!({})).unwrap();
        steps(&mut h);
        key(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(h.state().ask_agent.bubble.as_ref().map(|b| b.text.as_str()), Some("make it formal"));
        key(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(hub.listen(Duration::from_millis(10), || true).map(|r| r.instruction), Some("make it formal".to_string()));
    }

    /// A waiting agent gets a request as soon as it is made: `agent.listen` is woken, it doesn't
    /// poll or wait for the window's next frame.
    #[test]
    fn a_waiting_agent_gets_the_request_at_once() {
        let hub = AgentHub::new(7985, None);
        let agent = hub.clone();
        let waiting = std::thread::spawn(move || {
            let r = agent.listen(Duration::from_secs(20), || true);
            (r, std::time::Instant::now())
        });
        while !hub.agent_present() {
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(20));
        let asked = std::time::Instant::now();
        hub.ask("go", json!({}), json!({})).unwrap();
        let (r, got) = waiting.join().unwrap();
        assert!(r.is_some());
        let lag = got.duration_since(asked);
        assert!(lag < Duration::from_millis(500), "woken in {lag:?}");
    }

    /// A quick action is one click: its instruction, id and `apply` reach the agent; a submenu
    /// (Translate) works the same way.
    #[test]
    fn quick_actions_send_their_instruction_and_apply() {
        use egui_kittest::kittest::Queryable;
        let hub = AgentHub::new(7985, None);
        hub.listen(Duration::from_millis(1), || true);
        let mut h = window(Some(hub.clone()));
        h.state_mut().run("select.text", json!({"text": "the clay slowly"})).unwrap();
        h.state_mut().run("ui.askAgent", json!({})).unwrap();
        steps(&mut h);
        let shorten = h.get_by_label("Shorten").rect().center();
        click(&mut h, shorten);
        let r = hub.listen(Duration::from_millis(10), || true).expect("Shorten sent a request");
        let v = r.to_json();
        assert_eq!((v["action"]["id"].as_str(), v["apply"].as_str()), (Some("shorten"), Some("replace")), "{v}");
        assert!(v["instruction"].as_str().unwrap().starts_with("Shorten the selected text"), "{v}");
        assert_eq!(v["selection"]["text"], "the clay slowly");
        assert_eq!(v["review"], "trackedChanges");
        assert!(h.state().ask_agent.history.is_empty(), "actions don't crowd the recent instructions");
        // Translate › French, from the submenu.
        h.state_mut().run("ui.askAgent", json!({"close": true})).unwrap();
        h.state_mut().run("ui.askAgent", json!({})).unwrap();
        steps(&mut h);
        let translate = h.get_by_label("Translate").rect().center();
        click(&mut h, translate);
        let french = h.get_by_label("French").rect().center();
        click(&mut h, french);
        let r = hub.listen(Duration::from_millis(10), || true).expect("Translate › French sent a request");
        assert_eq!(r.action.as_ref().map(|a| a.id.as_str()), Some("translate.fr"));
        // Scripts send actions by id; an unknown one is refused.
        let id = h.state_mut().run("ui.askAgent", json!({"action": "explain"})).unwrap()["id"].as_u64().unwrap();
        assert_eq!(hub.request(id).unwrap().apply, Apply::ReplyOnly);
        assert!(h.state_mut().run("ui.askAgent", json!({"action": "nope"})).unwrap_err().contains("no quick action"));
    }

    /// What an agent does for a request (`engine.execute` with `request`) arrives as tracked
    /// changes under its name, Track Changes off or on, and the person's own name and setting are
    /// back afterwards. Edits are refused once the person cancelled, and for answer-only requests.
    #[test]
    fn edits_for_a_request_are_the_agents_tracked_changes() {
        let ctx = egui::Context::default();
        let hub = AgentHub::new(7985, None);
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), Default::default()).with_agent_hub(hub.clone());
        a.run("text.insert", json!({"text": "Kilns fire clay."})).unwrap();
        a.session.author = "Ana Writer".into();
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"action": "rewrite.professional"})).unwrap()["id"].as_u64().unwrap();
        let taken = hub.listen_as(Duration::from_millis(10), Some("  test\u{7}client  bot "), || true).unwrap();
        assert_eq!((taken.agent.as_deref(), taken.author()), (Some("testclient bot"), "Agent (testclient bot)".to_string()));
        let exec = |a: &mut WordApp, command: &str, params: Value| {
            let (req, _) = crate::ControlRequest::new("engine.execute", json!({"command": command, "params": params, "request": id}));
            match crate::control::handle(a, &ctx, &req) {
                crate::control::Outcome::Done(v) => v,
                crate::control::Outcome::Screenshot { .. } => Value::Null,
            }
        };
        let r = exec(&mut a, "text.insert", json!({"text": "stoneware"}));
        assert_eq!(r["ok"], true, "{r}");
        assert!(!a.session.doc.settings.track_changes, "the person's setting is untouched");
        assert!(!a.session.force_tracking);
        assert_eq!(a.session.author, "Ana Writer", "the person's name is back");
        assert!(
            !a.session.doc.revisions.is_empty() && a.session.doc.revisions.iter().all(|r| r.author == "Agent (testclient bot)"),
            "tracked as the agent"
        );
        assert_eq!(a.session.doc.plain_text(wordcraft_doc::StoryRef::Body), "Kilns fire stonewareclay.", "a revision to accept or reject");
        // Cancelled: refused, nothing changes.
        hub.cancel(id);
        let before = a.session.doc.plain_text(wordcraft_doc::StoryRef::Body);
        let r = exec(&mut a, "text.insert", json!({"text": "!"}));
        assert!(r["error"].as_str().unwrap_or("").contains("cancelled by the person"), "{r}");
        assert_eq!(a.session.doc.plain_text(wordcraft_doc::StoryRef::Body), before);
        // Answer only (Explain): reading and selecting work, editing doesn't.
        let id = a.run("ui.askAgent", json!({"action": "explain"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        let exec2 = |a: &mut WordApp, command: &str, params: Value| {
            let (req, _) = crate::ControlRequest::new("engine.execute", json!({"command": command, "params": params, "request": id}));
            match crate::control::handle(a, &ctx, &req) {
                crate::control::Outcome::Done(v) => v,
                crate::control::Outcome::Screenshot { .. } => Value::Null,
            }
        };
        assert_eq!(exec2(&mut a, "select.text", json!({"text": "Kilns"}))["ok"], true);
        let r = exec2(&mut a, "text.insert", json!({"text": "Ovens"}));
        assert!(r["error"].as_str().unwrap_or("").contains("answer only"), "{r}");
        // No such request.
        let (req, _) = crate::ControlRequest::new("engine.execute", json!({"command": "text.insert", "params": {"text": "x"}, "request": 999}));
        let crate::control::Outcome::Done(r) = crate::control::handle(&mut a, &ctx, &req) else { return };
        assert!(r["error"].as_str().unwrap_or("").contains("no Ask Agent request 999"), "{r}");
    }

    /// The agent's edit for a request arrives as a tracked change, with Track Changes off (before
    /// quick actions and tracked edits, it went in as a plain edit).
    #[test]
    fn an_agents_edit_for_a_request_is_a_tracked_change() {
        let ctx = egui::Context::default();
        let hub = AgentHub::new(7985, None);
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), Default::default()).with_agent_hub(hub.clone());
        a.run("text.insert", json!({"text": "Kilns fire clay."})).unwrap();
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "use a better word"})).unwrap()["id"].as_u64().unwrap();
        assert!(hub.listen(Duration::from_millis(10), || true).is_some());
        let (req, _) =
            crate::ControlRequest::new("engine.execute", json!({"command": "text.insert", "params": {"text": "stoneware"}, "request": id}));
        let _ = crate::control::handle(&mut a, &ctx, &req);
        assert!(!a.session.doc.revisions.is_empty(), "the agent's edit is a tracked change");
        assert!(!a.session.doc.settings.track_changes, "the person's setting is untouched");
    }

    // Stage (c): proposals, Accept / Discard / Retry.

    fn app_with(hub: &AgentHub, text: &str) -> WordApp {
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), Default::default()).with_agent_hub(hub.clone());
        a.run("text.insert", json!({"text": text})).unwrap();
        a.session.author = "Ana Writer".into();
        a
    }

    fn body(a: &WordApp) -> String {
        a.session.doc.plain_text(wordcraft_doc::StoryRef::Body)
    }

    fn proposal(text: &str) -> Option<Proposal> {
        Some(Proposal { text: text.into(), markdown: false })
    }

    /// An agent's `engine.execute` for request `id`.
    fn exec_for(a: &mut WordApp, id: u64, command: &str, params: Value) -> Value {
        let ctx = egui::Context::default();
        let (req, _) = crate::ControlRequest::new("engine.execute", json!({"command": command, "params": params, "request": id}));
        match crate::control::handle(a, &ctx, &req) {
            crate::control::Outcome::Done(v) => v,
            crate::control::Outcome::Screenshot { .. } => Value::Null,
        }
    }

    #[test]
    fn word_diff_marks_what_goes_and_what_comes() {
        let d = word_diff("Kilns fire the clay slowly.", "Kilns fire clay fast.").unwrap();
        assert_eq!(
            d,
            vec![
                (Change::Same, "Kilns fire ".to_string()),
                (Change::Removed, "the ".to_string()),
                (Change::Same, "clay ".to_string()),
                (Change::Removed, "slowly".to_string()),
                (Change::Added, "fast".to_string()),
                (Change::Same, ".".to_string()),
            ]
        );
        assert_eq!(word_diff("", "New text").unwrap(), vec![(Change::Added, "New text".to_string())]);
        assert_eq!(word_diff("été à Montréal", "été").unwrap()[0], (Change::Same, "été".to_string()), "letters beyond ASCII are words");
        // Too long to compare: no comparison, never a huge table.
        let long = "word ".repeat(2000);
        assert!(word_diff(&long, &long).is_none());
    }

    /// The queue: a proposal streams (each reply replaces the text so far), closes with `done`, and
    /// is refused for an answer-only request or one the agent already edited; Retry asks again
    /// with `retryOf`.
    #[test]
    fn proposals_stream_and_retry_asks_again() {
        let hub = AgentHub::new(7985, None);
        let id = hub.ask("shorten", sel("Kilns fire the clay slowly."), json!({})).unwrap();
        let r = hub.listen(Duration::from_millis(10), || true).unwrap();
        assert_eq!(r.to_json()["proposalFormats"], json!(["text", "markdown"]));
        assert_eq!(r.to_json()["retryOf"], Value::Null);
        assert!(hub.answer(id, None, None, false).is_err(), "neither a note nor a proposal");
        hub.answer(id, Some("Shortening…"), proposal("Kilns"), false).unwrap();
        hub.answer(id, None, proposal("Kilns fire clay."), false).unwrap();
        let r = hub.request(id).unwrap();
        assert_eq!((r.state, r.reply.as_str(), r.proposal.unwrap().text.as_str()), (State::Working, "Shortening…", "Kilns fire clay."));
        assert_eq!(hub.request(id).unwrap().review(), None, "nothing to review while it streams");
        hub.answer(id, Some("Done."), None, true).unwrap();
        assert_eq!(hub.request(id).unwrap().review(), Some(Review::Proposal), "the last proposal is the final one");
        // A huge proposal is cut at a character boundary.
        let big = hub.ask("expand", sel("x"), json!({})).unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(big, None, proposal(&"é".repeat(MAX_PROPOSAL_BYTES)), false).unwrap();
        assert!(hub.request(big).unwrap().proposal.unwrap().text.len() <= MAX_PROPOSAL_BYTES);
        // Answer only: no proposal.
        let explain = hub.ask_with("", crate::agent_actions::find("explain").as_ref(), sel("glaze"), json!({})).unwrap();
        let r = hub.listen(Duration::from_millis(10), || true).unwrap();
        assert_eq!(r.to_json()["proposalFormats"], json!([]));
        assert!(hub.answer(explain, None, proposal("x"), true).unwrap_err().contains("answer only"));
        // Edited already: no proposal on top.
        let edited = hub.ask("fix", sel("teh"), json!({})).unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.add_revisions(edited, [3, 4, 3]);
        assert_eq!(hub.request(edited).unwrap().revisions, vec![3, 4]);
        assert!(hub.answer(edited, None, proposal("the"), true).unwrap_err().contains("already changed the document"));
        // Retry: the same request again, saying which one it retries; the old one is put away.
        let again = hub.retry(id).unwrap();
        assert!(hub.request(id).is_none());
        let r = hub.listen(Duration::from_millis(10), || true).unwrap();
        assert_eq!((r.id, r.instruction.as_str(), r.retry_of), (again, "shorten", Some(id)));
        assert_eq!(r.to_json()["retryOf"], id);
        assert_eq!(r.selection["text"], "Kilns fire the clay slowly.");
        assert!(r.proposal.is_none() && r.revisions.is_empty());
        // Proposal params as the control channel gets them.
        assert_eq!(
            Proposal::from_params(&json!({"proposal": "a\r\nb", "format": "markdown"})).unwrap(),
            Some(Proposal { text: "a\nb".into(), markdown: true })
        );
        assert_eq!(Proposal::from_params(&json!({"text": "note"})).unwrap(), None);
        assert!(Proposal::from_params(&json!({"proposal": ["x"]})).is_err());
        assert!(Proposal::from_params(&json!({"proposal": "x", "format": "html"})).is_err());
    }

    /// Accept puts the proposal in place of the text the person asked about, wherever the caret
    /// went meanwhile, as one Undo step and a tracked change by the agent; the person's name and
    /// Track Changes setting stay theirs. Accepting while the agent still writes is refused.
    #[test]
    fn accepting_a_proposal_puts_it_in_place_as_the_agents_tracked_change() {
        let hub = AgentHub::new(7985, None);
        let mut a = app_with(&hub, "Kilns fire the clay slowly over many hours.");
        a.run("select.text", json!({"text": "the clay slowly"})).unwrap();
        let id = a.run("ui.askAgent", json!({"action": "shorten"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("test bot"), || true).unwrap();
        hub.answer(id, None, proposal("the clay"), false).unwrap();
        assert!(a.run("ui.askAgent", json!({"accept": id})).unwrap_err().contains("still being worked on"));
        hub.answer(id, Some("Cut a word."), proposal("clay"), true).unwrap();
        // The person moved on; the proposal still goes where they asked.
        a.run("caret.docEnd", json!({})).unwrap();
        let depth = a.session.undo_depth();
        let r = a.run("ui.askAgent", json!({"accept": id})).unwrap();
        assert_eq!((r["accepted"].as_u64(), r["author"].as_str()), (Some(id), Some("Agent (test bot)")));
        assert_eq!(a.session.undo_depth(), depth + 1, "one Undo step");
        assert!(a.session.doc.revisions.iter().all(|r| r.author == "Agent (test bot)"), "tracked as the agent");
        assert!(!a.session.doc.settings.track_changes && !a.session.force_tracking && a.session.revision_base == 0);
        assert_eq!(a.session.author, "Ana Writer");
        assert!(hub.request(id).is_none(), "accepted: put away");
        a.run("review.acceptAll", json!({})).unwrap();
        assert_eq!(body(&a), "Kilns fire clay over many hours.");
        a.run("edit.undo", json!({})).unwrap();
        a.run("edit.undo", json!({})).unwrap();
        assert_eq!(body(&a), "Kilns fire the clay slowly over many hours.", "one Undo takes the whole proposal back");
        // insertBelow: new paragraphs after the selection's, which stays; Markdown brings its list.
        a.run("select.text", json!({"text": "Kilns"})).unwrap();
        let id = a.run("ui.askAgent", json!({"action": "summarize"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(id, None, Some(Proposal { text: "- slow firing\n- even heat".into(), markdown: true }), true).unwrap();
        let depth = a.session.undo_depth();
        a.run("ui.askAgent", json!({"accept": id})).unwrap();
        assert_eq!(a.session.undo_depth(), depth + 1);
        let d = a.session.run("document.inspect", &json!({})).unwrap();
        let blocks = d["blocks"].as_array().unwrap();
        let texts: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
        assert_eq!(texts, vec!["Kilns fire the clay slowly over many hours.", "slow firing", "even heat"], "{d}");
        assert!(blocks.iter().filter(|b| b["text"] == "slow firing").all(|b| !b["list"].is_null()), "a list item: {d}");
    }

    /// The text changed since the request: found again when it is in one place only, else Accept
    /// is refused, and the document, the selection and Undo are as they were.
    #[test]
    fn accept_finds_the_text_again_or_says_it_changed() {
        let hub = AgentHub::new(7985, None);
        let mut a = app_with(&hub, "Kilns fire clay.");
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "a better word"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(id, None, proposal("stoneware"), true).unwrap();
        // The person typed before it: same text, another place.
        a.run("caret.docStart", json!({})).unwrap();
        a.run("text.insert", json!({"text": "Hot "})).unwrap();
        a.run("ui.askAgent", json!({"accept": id})).unwrap();
        a.run("review.acceptAll", json!({})).unwrap();
        assert_eq!(body(&a), "Hot Kilns fire stoneware.");
        // Now the text is gone (typed over): refused, nothing changes.
        a.run("select.text", json!({"text": "fire"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "stronger"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(id, None, proposal("blaze"), true).unwrap();
        a.run("select.text", json!({"text": "fire"})).unwrap();
        a.run("text.insert", json!({"text": "heat"})).unwrap();
        a.run("caret.docEnd", json!({})).unwrap();
        let (before, depth, sel) = (body(&a), a.session.undo_depth(), a.session.sel.clone());
        let e = a.run("ui.askAgent", json!({"accept": id})).unwrap_err();
        assert!(e.contains("has changed since"), "{e}");
        assert_eq!((body(&a), a.session.undo_depth(), a.session.sel.clone()), (before, depth, sel), "nothing changed");
        assert!(hub.request(id).is_some(), "still there: Discard or Retry");
        // Moved, and now twice in the document: which one is unclear, refused too.
        a.run("select.text", json!({"text": "heat"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "warmer"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(id, None, proposal("warmth"), true).unwrap();
        a.run("caret.docStart", json!({})).unwrap();
        a.run("text.insert", json!({"text": "Some heat. "})).unwrap();
        assert!(a.run("ui.askAgent", json!({"accept": id})).unwrap_err().contains("has changed since"));
        assert!(a.run("ui.askAgent", json!({"discard": id})).unwrap()["discarded"] == id);
        assert!(hub.request(id).is_none());
    }

    /// The agent's own edits: Accept makes exactly this request's tracked changes final, Discard
    /// rejects exactly them, one Undo step each; another request's changes and the person's stay.
    /// A request with a proposal takes no edits.
    #[test]
    fn accept_and_discard_resolve_only_the_requests_own_edits() {
        let hub = AgentHub::new(7985, None);
        let mut a = app_with(&hub, "Kilns fire clay. Glazes melt.");
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let first = a.run("ui.askAgent", json!({"instruction": "better word"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("bot"), || true).unwrap();
        assert_eq!(exec_for(&mut a, first, "select.text", json!({"text": "clay"}))["ok"], true);
        assert_eq!(exec_for(&mut a, first, "text.insert", json!({"text": "stoneware"}))["ok"], true);
        hub.reply(first, "Changed one word.", true).unwrap();
        a.run("select.text", json!({"text": "Glazes"})).unwrap();
        let second = a.run("ui.askAgent", json!({"instruction": "better word"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("bot"), || true).unwrap();
        exec_for(&mut a, second, "select.text", json!({"text": "Glazes"}));
        exec_for(&mut a, second, "text.insert", json!({"text": "Slips"}));
        hub.reply(second, "Changed one word.", true).unwrap();
        let (r1, r2) = (hub.request(first).unwrap(), hub.request(second).unwrap());
        assert_eq!((r1.review(), r2.review()), (Some(Review::Edits), Some(Review::Edits)));
        assert!(r1.revisions.iter().all(|r| !r2.revisions.contains(r)), "each request has its own revisions");
        assert!(hub.answer(first, None, proposal("x"), true).is_err());
        assert_eq!(body(&a), "Kilns fire stonewareclay. SlipsGlazes melt.");
        let depth = a.session.undo_depth();
        a.run("ui.askAgent", json!({"accept": first})).unwrap();
        assert_eq!(body(&a), "Kilns fire stoneware. SlipsGlazes melt.", "only the first request's changes are final");
        a.run("ui.askAgent", json!({"discard": second})).unwrap();
        assert_eq!(body(&a), "Kilns fire stoneware. Glazes melt.");
        assert_eq!(a.session.undo_depth(), depth + 2, "one Undo step each");
        assert!(hub.requests().is_empty());
        // With a proposal, the agent's edits are refused.
        a.run("select.text", json!({"text": "melt"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "stronger"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(id, None, proposal("flow"), false).unwrap();
        let r = exec_for(&mut a, id, "text.insert", json!({"text": "run"}));
        assert!(r["error"].as_str().unwrap_or("").contains("has a proposal"), "{r}");
        assert_eq!(exec_for(&mut a, id, "select.text", json!({"text": "melt"}))["ok"], true, "reading and selecting still work");
        // Retry after edits: they are rejected (one Undo step) and the request goes out again.
        hub.cancel(id);
        a.run("select.text", json!({"text": "Glazes"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "plural"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        exec_for(&mut a, id, "select.text", json!({"text": "Glazes"}));
        exec_for(&mut a, id, "text.insert", json!({"text": "Glaze"}));
        hub.reply(id, "Done.", true).unwrap();
        let new = a.run("ui.askAgent", json!({"retry": id})).unwrap()["id"].as_u64().unwrap();
        assert_eq!(body(&a), "Kilns fire stoneware. Glazes melt.", "the first try is rejected");
        assert_eq!(hub.listen(Duration::from_millis(10), || true).map(|r| r.retry_of), Some(Some(id)));
        assert!(a.run("ui.askAgent", json!({"accept": new})).is_err(), "nothing to accept yet");
        assert!(a.run("ui.askAgent", json!({"accept": "x"})).is_err());
    }

    /// The bubble: the proposal grows as the agent writes it; once done, Accept, Discard and Retry
    /// show, Accept has the keyboard (Enter accepts), Escape discards and Ctrl+R asks again.
    #[test]
    fn the_bubble_streams_the_proposal_and_takes_enter_escape_and_ctrl_r() {
        use egui_kittest::kittest::Queryable;
        let hub = AgentHub::new(7985, None);
        hub.listen(Duration::from_millis(1), || true);
        let mut h = window(Some(hub.clone()));
        h.state_mut().run("select.text", json!({"text": "the clay slowly"})).unwrap();
        h.state_mut().run("ui.askAgent", json!({})).unwrap();
        steps(&mut h);
        h.event(egui::Event::Text("shorter".into()));
        steps(&mut h);
        key(&mut h, Key::Enter, Modifiers::NONE);
        let r = hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(r.id, Some("Writing…"), proposal("the clay"), false).unwrap();
        steps(&mut h);
        assert!(h.query_by_label_contains("the clay").is_some(), "the partial proposal shows");
        assert_eq!(h.query_all_by_label("Accept").count(), 0, "nothing to accept while it streams");
        hub.answer(r.id, Some("Shorter."), proposal("the clay, slowly"), true).unwrap();
        steps(&mut h);
        for label in ["Accept", "Discard", "Retry"] {
            assert_eq!(h.query_all_by_label(label).count(), 1, "{label}");
        }
        key(&mut h, Key::Enter, Modifiers::NONE);
        assert!(hub.request(r.id).is_none(), "Enter accepted");
        assert!(h.state().ask_agent.bubble.is_none());
        h.state_mut().run("review.acceptAll", json!({})).unwrap();
        assert_eq!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body), "Kilns fire the clay, slowly over many hours.");

        // Escape discards: the document doesn't change.
        h.state_mut().run("select.text", json!({"text": "Kilns"})).unwrap();
        let id = h.state_mut().run("ui.askAgent", json!({"instruction": "warmer"})).unwrap()["id"].as_u64().unwrap();
        h.state_mut().ask_agent.bubble = Some(Bubble { request: Some(id), ..Bubble::at(h.state().session.sel.ordered().1) });
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(id, None, proposal("Ovens"), true).unwrap();
        steps(&mut h);
        key(&mut h, Key::Escape, Modifiers::NONE);
        assert!(hub.request(id).is_none() && h.state().ask_agent.bubble.is_none());
        assert!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body).starts_with("Kilns"), "discarded");

        // Ctrl+R asks again; the bubble follows the new request.
        let id = h.state_mut().run("ui.askAgent", json!({"instruction": "warmer"})).unwrap()["id"].as_u64().unwrap();
        h.state_mut().ask_agent.bubble = Some(Bubble { request: Some(id), ..Bubble::at(h.state().session.sel.ordered().1) });
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.answer(id, None, proposal("Ovens"), true).unwrap();
        steps(&mut h);
        key(&mut h, Key::R, Modifiers::COMMAND);
        let again = hub.listen(Duration::from_millis(10), || true).expect("asked again");
        assert_eq!(again.retry_of, Some(id));
        assert_eq!(h.state().ask_agent.bubble.as_ref().and_then(|b| b.request), Some(again.id));

        // A key pressed while the agent works: the result doesn't take the keyboard, Enter is the
        // document's again.
        hub.answer(again.id, Some("…"), None, false).unwrap();
        steps(&mut h);
        key(&mut h, Key::ArrowRight, Modifiers::NONE);
        hub.answer(again.id, None, proposal("Ovens"), true).unwrap();
        steps(&mut h);
        key(&mut h, Key::Enter, Modifiers::NONE);
        assert!(hub.request(again.id).is_some(), "not accepted by a key meant for the document");
        // Nor discarded by an Escape meant for the document: the bubble closes, the result waits
        // in the corner card, with its buttons.
        key(&mut h, Key::Escape, Modifiers::NONE);
        assert!(h.state().ask_agent.bubble.is_none() && hub.request(again.id).is_some());
        assert_eq!(h.query_all_by_label("Accept").count(), 1, "the corner card offers Accept");
        let discard = h.get_by_label("Discard").rect().center();
        click(&mut h, discard);
        assert!(hub.request(again.id).is_none());
    }
}
