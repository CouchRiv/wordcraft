//! Ask Agent (gap #25): the person selects text, picks **Ask Agent…** (Help › Agents, the page's
//! context menu, Ctrl+Shift+O / ⌘⇧O) and types an instruction ("shorten this paragraph"). An agent
//! they already run on the same computer, connected through the control channel
//! (`wordcraft-cli mcp --connect`), takes the request with `agent.listen`, does the work with the
//! usual commands and answers with `agent.reply`; the answer shows in the window. WordCraft calls
//! no model and holds no key: the "provider" is whatever agent the person connected.
//!
//! [`AgentHub`] is the queue between the window and the control channel. The control server's
//! connection threads share it, so `agent.listen` waits there, never on the UI thread; the window
//! reads it every frame and is woken when it changes. See `docs/control-protocol.md` (Ask Agent).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use egui::{Align2, Key, Modifiers, Order, Pos2, Rect, Vec2, pos2, vec2};
use serde_json::{Value, json};
use wordcraft_doc::Pos;

use crate::WordApp;
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
const WIDTH: f32 = 320.0;

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
    pub instruction: String,
    /// `{text, anchor, focus, truncated}` as it was when the person asked.
    pub selection: Value,
    /// `{title}`: the file name, else the document's title, else null.
    pub document: Value,
    pub state: State,
    /// The agent's latest text: its answer, or a progress note while it works.
    pub reply: String,
    /// When an agent took it (`now_ms`).
    taken_ms: f64,
}

impl Request {
    /// What `agent.listen` hands the agent.
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "instruction": self.instruction, "selection": self.selection, "document": self.document})
    }
}

#[derive(Default)]
struct Inner {
    requests: VecDeque<Request>,
    next_id: u64,
    /// `agent.listen` calls waiting right now.
    listeners: usize,
    /// When an agent last listened or replied (`now_ms`); 0 = never.
    last_seen_ms: f64,
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

    /// Queue a request; its id. An instruction that is empty once trimmed is refused.
    pub fn ask(&self, instruction: &str, selection: Value, document: Value) -> Result<u64, String> {
        let instruction = cap_chars(instruction.trim(), MAX_TEXT_CHARS);
        if instruction.is_empty() {
            return Err("type an instruction for the agent first".into());
        }
        let id = {
            let mut s = self.lock();
            let id = s.next_id;
            s.next_id = s.next_id.saturating_add(1);
            s.requests.push_back(Request { id, instruction, selection, document, state: State::Waiting, reply: String::new(), taken_ms: 0.0 });
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
        Ok(id)
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
        let text = cap_chars(text.trim(), MAX_TEXT_CHARS);
        let r = {
            let mut s = self.lock();
            s.last_seen_ms = crate::now_ms();
            match s.requests.iter_mut().find(|r| r.id == id) {
                None => Err(format!("no Ask Agent request {id}: it was never made, or the person dismissed it")),
                Some(r) if r.state == State::Cancelled => Ok(json!({"id": id, "cancelled": true})),
                Some(r) if r.state == State::Answered => Err(format!("Ask Agent request {id} is already answered")),
                Some(r) => {
                    r.reply = text;
                    r.state = if done { State::Answered } else { State::Working };
                    if r.taken_ms == 0.0 {
                        r.taken_ms = crate::now_ms();
                    }
                    Ok(json!({"id": id, "shown": true, "done": done}))
                }
            }
        };
        self.changed();
        r
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
}

/// The bubble at the selection.
pub struct Bubble {
    /// The selection's end when it opened: the bubble stays there while the document scrolls.
    pub anchor: Pos,
    /// What the person types.
    pub text: String,
    /// The request sent from it; `None` while the person types.
    pub request: Option<u64>,
    /// The box has taken the keyboard once.
    focused: bool,
}

impl Bubble {
    fn at(anchor: Pos) -> Self {
        Self { anchor, text: String::new(), request: None, focused: false }
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
            .map(|r| json!({"id": r.id, "state": r.state.name(), "instruction": r.instruction, "reply": r.reply}))
            .collect();
        json!({
            "available": self.unavailable().is_none(),
            "reason": self.unavailable().map(Unavailable::english),
            "agentListening": self.hub.as_ref().is_some_and(AgentHub::agent_present),
            "bubble": self.bubble.as_ref().map(|b| json!({"request": b.request})),
            "history": self.history,
            "requests": requests,
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

/// The document as an agent gets it: its file name, else its title (never translated text), and
/// the language of the selected text (the document's when the selection mixes languages).
fn document(app: &mut WordApp) -> Value {
    let lang = app.session.run("format.state", &json!({})).ok().and_then(|v| v.get("lang").and_then(Value::as_str).map(str::to_string));
    let s = &app.session;
    let title = s
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .or_else(|| (!s.doc.core.title.trim().is_empty()).then(|| s.doc.core.title.clone()));
    json!({"title": title, "language": lang.or_else(|| s.doc.styles.default_chr.lang.clone())})
}

/// `ui.askAgent`. No params: open the bubble at the selection (refused, with the reason, when no
/// agent is listening). `instruction`: send it without the box. `cancel`: cancel that request.
/// `close`: close the bubble.
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
    let Some(hub) = app.ask_agent.hub.clone() else {
        let why = app.ask_agent.unavailable().unwrap_or(Unavailable::NoControlChannel);
        app.status(why.shown());
        return Err(why.english());
    };
    if let Some(text) = p.get("instruction") {
        let text = text.as_str().ok_or("`instruction` is text")?;
        let (selection, document) = (selection(app), document(app));
        let id = hub.ask(text, selection, document)?;
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
    let pos = bubble_pos(at, vec2(WIDTH, 170.0), viewport)?;
    let chars = app.session.selected_text().chars().count();
    let present = hub.as_ref().is_some_and(AgentHub::agent_present);
    let animate = app.animations_on();
    let edit_id = egui::Id::new("wc_ask_agent_text");
    let typing = ctx.memory(|m| m.has_focus(edit_id));
    let mut act = BubbleAct::None;
    let shown = egui::Area::new(egui::Id::new("wc_ask_agent")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_width(WIDTH);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(tl!("Ask Agent")).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text(tl!("Close")).clicked() {
                        act = BubbleAct::Close;
                    }
                });
            });
            match &current {
                None => {
                    let about = if chars == 0 {
                        tl!("No text is selected: the agent gets the caret's place.").to_string()
                    } else {
                        crate::i18n::fmt(tl!("The agent gets the selected text ({count} characters)."), &[("count", &chars.to_string())])
                    };
                    ui.label(egui::RichText::new(about).small().weak());
                    // Enter sends, Shift+Enter starts a new line.
                    let enter = typing && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                    let history = app.ask_agent.history.clone();
                    let Some(b) = app.ask_agent.bubble.as_mut() else { return };
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
                }
                Some(r) => {
                    let t = Tokens::get(ui.ctx());
                    ui.add(egui::Label::new(egui::RichText::new(&r.instruction).small().color(t.text_dim)).truncate());
                    status(ui, r, present, animate);
                    ui.horizontal(|ui| {
                        if r.state.open() {
                            if ui.button(tl!("Cancel")).clicked() {
                                act = BubbleAct::Cancel(r.id);
                            }
                        } else if ui.button(tl!("Close")).clicked() {
                            act = BubbleAct::Dismiss(r.id);
                        }
                    });
                }
            }
        });
    });
    // Escape, or a press in the document outside the bubble, closes it (a request it sent goes on
    // in the corner card; an answered one is put away).
    let rect = shown.response.rect;
    let escape = ctx.input(|i| i.key_pressed(Key::Escape));
    let press = ctx.input(|i| if i.pointer.any_pressed() { i.pointer.press_origin() } else { None });
    let elsewhere =
        press.is_some_and(|p| viewport.contains(p) && !rect.contains(p) && !ctx.layer_id_at(p).is_some_and(|l| l.order >= Order::Foreground));
    if matches!(act, BubbleAct::None) && (escape || elsewhere) {
        act = match &current {
            Some(r) if !r.state.open() => BubbleAct::Dismiss(r.id),
            _ => BubbleAct::Close,
        };
    }
    match act {
        BubbleAct::None => {}
        BubbleAct::Send => send(app),
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
    }
    request
}

enum BubbleAct {
    None,
    Send,
    Close,
    Cancel(u64),
    Dismiss(u64),
}

/// A request's state: a spinner while it waits or the agent works (a still one with animations
/// off), the agent's text, or Cancelled.
fn status(ui: &mut egui::Ui, r: &Request, present: bool, animate: bool) {
    let busy = |ui: &mut egui::Ui, text: &str| {
        ui.horizontal(|ui| {
            if animate {
                ui.add(egui::Spinner::new().size(12.0));
            }
            ui.label(text);
        });
    };
    match r.state {
        State::Waiting => {
            busy(ui, tl!("Waiting for the agent…"));
            if !present {
                ui.label(egui::RichText::new(tl!("No agent is listening right now.")).small().weak());
            }
        }
        State::Working if r.reply.is_empty() => busy(ui, tl!("The agent is working…")),
        // A progress note while the agent works.
        State::Working => busy(ui, &r.reply),
        // The agent's own words: shown as it wrote them, selectable to copy.
        State::Answered if r.reply.is_empty() => drop(ui.label(tl!("Done."))),
        State::Answered => drop(ui.add(egui::Label::new(r.reply.as_str()).selectable(true).wrap())),
        State::Cancelled => drop(ui.label(egui::RichText::new(tl!("Cancelled")).weak())),
    }
}

/// The requests the bubble doesn't show, newest last, in the canvas's bottom-right corner.
fn show_requests(app: &mut WordApp, ctx: &egui::Context, viewport: Rect, in_bubble: Option<u64>) {
    let Some(hub) = app.ask_agent.hub.clone() else { return };
    let requests: Vec<Request> = hub.requests().into_iter().filter(|r| Some(r.id) != in_bubble).collect();
    if requests.is_empty() {
        return;
    }
    let present = hub.agent_present();
    let animate = app.animations_on();
    let t = Tokens::get(ctx);
    egui::Area::new(egui::Id::new("wc_ask_agent_requests"))
        .order(Order::Foreground)
        .pivot(Align2::RIGHT_BOTTOM)
        .fixed_pos(viewport.right_bottom() - vec2(16.0, 16.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(WIDTH);
                ui.label(egui::RichText::new(tl!("Ask Agent")).strong());
                for r in requests.iter().rev().take(SHOWN).rev() {
                    ui.separator();
                    ui.add(egui::Label::new(egui::RichText::new(&r.instruction).small().color(t.text_dim)).truncate());
                    status(ui, r, present, animate);
                    if r.state.open() {
                        if ui.small_button(tl!("Cancel")).clicked() {
                            hub.cancel(r.id);
                        }
                    } else if ui.small_button(tl!("Dismiss")).clicked() {
                        hub.dismiss(r.id);
                    }
                }
            });
        });
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
        let r = hub.listen(Duration::from_millis(10), || true).expect("the request is queued");
        assert_eq!(r.instruction, "make it formal");
        assert_eq!(r.selection["text"], "the clay slowly");
        assert_eq!(r.document["language"], "en-US");
        assert_eq!(h.state().ask_agent.history, vec!["make it formal".to_string()]);
        steps(&mut h);
        assert!(h.query_by_label_contains("The agent is working").is_some(), "the bubble follows the request");
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
}
