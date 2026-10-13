//! Several agents on one document (Ask Agent, gap #25): who has which request, the text each
//! request covers, and the agent's closing comment.
//!
//! - **Its text, followed.** From the moment the person sends a request, the window keeps its
//!   selection as a range lock in the engine ([`wordcraft_engine::locks`]), which moves with every
//!   edit (the agent's, the person's, another agent's, Undo).
//! - **Held for the agent.** While an agent works on the request, the range is held for it (owner
//!   `Agent (<name>)`): another agent's edit inside it is refused with an error naming the holder.
//!   The window shows the range in the agent's colour with its name. The person is never refused,
//!   and can release the range or cancel the request.
//! - **Closing comment.** When the agent answers with a `comment` (`agent.reply`,
//!   `reply_to_request`), the comment is anchored on the range as it is then, which covers what the
//!   agent changed, with the agent as its author. Then the range is let go.
//! - **The list.** The corner card lists every open request (agent and its colour, instruction,
//!   state) and the last answered ones: none is hidden while it is open.

use std::collections::BTreeMap;

use egui::{Align2, Color32, Order, Rect, pos2, vec2};
use serde_json::{Value, json};
use wordcraft_doc::Pos;

use crate::WordApp;
use crate::ask_agent::{Request, SHOWN, State, WIDTH, card, header, shown_title, status};
use crate::theme::Tokens;

/// What the window keeps about one request.
#[derive(Clone, Debug, Default)]
pub struct Tracked {
    /// Its range in the session's locks; `None` once let go (or when the document was replaced).
    pub lock: Option<u64>,
    /// The person released the range: it holds nobody back any more.
    pub released: bool,
    /// The closing comment's id in the document, once anchored.
    pub comment: Option<u32>,
    /// This request produced a successful story-block edit, not just a reply or selection.
    pub changed: bool,
    pub document_id: u64,
}

/// Request id → what the window keeps about it.
#[derive(Default)]
pub struct Queue {
    pub tracked: BTreeMap<u64, Tracked>,
}

/// An agent's colour on screen: the one its marks have on the page
/// ([`wordcraft_layout::display::agent_color`]); grey before an agent has the request.
pub fn color(r: &Request) -> Color32 {
    match &r.agent {
        Some(name) => crate::theme::c32(wordcraft_layout::display::agent_color(name)),
        None => Color32::from_rgb(0x8A, 0x8A, 0x8A),
    }
}

/// Start following request `id`'s text: the selection, as the request is made.
pub fn track(app: &mut WordApp, id: u64) {
    let (a, b) = (app.session.sel.anchor.clone(), app.session.sel.focus.clone());
    let lock = app.session.lock_range(None, &a, &b);
    app.ask_agent.queue.tracked.insert(id, Tracked { lock: Some(lock), document_id: app.session.document_id(), ..Default::default() });
}

/// The range a request covers now: its lock's, else its selection as it was sent (clamped).
pub fn range(app: &WordApp, r: &Request) -> Option<(Pos, Pos)> {
    if let Some(t) = app.ask_agent.queue.tracked.get(&r.id) {
        if t.document_id != app.session.document_id() {
            return None;
        }
        let l = t.lock.and_then(|l| app.session.locks.get(l))?;
        return Some((l.start.clone(), l.end.clone()));
    }
    let pos = |k: &str| r.selection.get(k).and_then(wordcraft_engine::cmd::parse_pos).map(|p| app.session.doc.clamp(&p));
    let (a, b) = (pos("anchor")?, pos("focus")?);
    Some(if a <= b { (a, b) } else { (b, a) })
}

/// Bring the ranges in line with the requests: follow requests made without the window (scripts),
/// hold a request's range for the agent that took it, anchor closing comments of answered requests,
/// let go of the ranges of closed and dismissed ones. Every frame, and before an agent's command.
pub fn sync(app: &mut WordApp) {
    let Some(hub) = app.ask_agent.hub.clone() else { return };
    let requests = hub.requests();
    for r in requests.iter().filter(|r| r.state.open()) {
        if let Some(t) = app.ask_agent.queue.tracked.get(&r.id) {
            if t.document_id != app.session.document_id() || t.lock.is_some_and(|l| app.session.locks.get(l).is_none()) {
                hub.cancel(r.id);
                release_lock(app, r.id);
            }
            continue;
        }
        let lock = range(app, r).map(|(a, b)| app.session.lock_range(None, &a, &b));
        app.ask_agent.queue.tracked.insert(r.id, Tracked { lock, document_id: app.session.document_id(), ..Default::default() });
    }
    let ids: Vec<u64> = app.ask_agent.queue.tracked.keys().copied().collect();
    for id in ids {
        let Some(t) = app.ask_agent.queue.tracked.get(&id).cloned() else { continue };
        match requests.iter().find(|r| r.id == id) {
            Some(r) if r.state == State::Waiting => {}
            Some(r) if r.state == State::Working => {
                if let Some(l) = t.lock {
                    let owner = (!t.released).then(|| r.author());
                    if app.session.locks.get(l).is_some_and(|x| x.owner != owner) {
                        app.session.locks.set_owner(l, owner.as_deref());
                    }
                }
            }
            Some(r) => {
                // Answered (its comment goes on the text first) or cancelled.
                if r.state == State::Answered && r.comment.is_some() && t.comment.is_none() {
                    let _ = anchor_comment(app, id);
                }
                release_lock(app, id);
            }
            None => {
                release_lock(app, id);
                app.ask_agent.queue.tracked.remove(&id);
            }
        }
    }
}

/// Let go of request `id`'s range.
fn release_lock(app: &mut WordApp, id: u64) {
    if let Some(l) = app.ask_agent.queue.tracked.get_mut(&id).and_then(|t| t.lock.take()) {
        app.session.locks.remove(l);
    }
}

/// Initials for an agent's comments: its name's (`claude-code` → `C`), else `AI`.
fn initials(name: Option<&str>) -> String {
    let i: String = name.unwrap_or("").split(|c: char| c.is_whitespace() || c == '-' || c == '_').filter_map(|w| w.chars().next()).take(3).collect();
    if i.is_empty() { "AI".into() } else { i.to_uppercase() }
}

/// Anchor request `id`'s closing comment on its range, with the agent as author: the comment's id
/// (`None` when the agent sent none). Once: a second call gives the same id. The person's
/// selection, name and Comments pane are left as they were.
pub fn anchor_comment(app: &mut WordApp, id: u64) -> Result<Option<u32>, String> {
    let r = app.ask_agent.hub.as_ref().and_then(|h| h.request(id)).ok_or_else(|| format!("no Ask Agent request {id}"))?;
    if r.state != State::Answered {
        return Err(format!("Ask Agent request {id} is not answered: its comment goes on the text with the answer"));
    }
    if let Some(c) = app.ask_agent.queue.tracked.get(&id).and_then(|t| t.comment) {
        return Ok(Some(c));
    }
    let Some(text) = r.comment.clone() else { return Ok(None) };
    if ["anchor", "focus"].iter().any(|key| r.selection.get(*key).and_then(wordcraft_engine::cmd::parse_pos).is_some_and(|pos| pos.path.0.len() > 1))
    {
        return Err(
            "Closing comments in tables need cell-level range tracking, which is not supported yet; the answer remains in the request list".into()
        );
    }
    if !app.ask_agent.queue.tracked.get(&id).is_some_and(|t| t.changed) {
        return Ok(None);
    }
    let (start, end) = range(app, &r).ok_or_else(|| format!("Ask Agent request {id} has no place in the document"))?;
    if start.path.0.len() > 1 || end.path.0.len() > 1 {
        return Err(
            "Closing comments in tables need cell-level range tracking, which is not supported yet; the answer remains in the request list".into()
        );
    }
    let s = &mut app.session;
    // The person's selection moves with the comment's marks: follow it like a request's range.
    let sel = s.sel.clone();
    let keep = s.lock_range(None, &sel.anchor, &sel.focus);
    let pane = s.view.comments_pane;
    s.sel = wordcraft_engine::Selection { anchor: start, focus: end };
    let author = std::mem::replace(&mut s.author, r.author());
    let person_initials = std::mem::replace(&mut s.initials, initials(r.agent.as_deref()));
    let done = s.run("review.newComment", &json!({"text": text}));
    s.author = author;
    s.initials = person_initials;
    s.view.comments_pane = pane;
    s.sel = match s.locks.remove(keep) {
        Some(l) if sel.anchor > sel.focus => wordcraft_engine::Selection { anchor: l.end, focus: l.start },
        Some(l) => wordcraft_engine::Selection { anchor: l.start, focus: l.end },
        None => sel,
    };
    s.clamp_selection();
    let comment = done.map_err(|e| e.to_string())?.get("id").and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok());
    if let Some(t) = app.ask_agent.queue.tracked.get_mut(&id) {
        t.comment = comment;
    } else {
        app.ask_agent.queue.tracked.insert(id, Tracked { comment, ..Default::default() });
    }
    Ok(comment)
}

/// Run `command` as agent `name` outside any request (`engine.execute {agent}`): not a tracked
/// change, but text another agent holds is refused.
pub fn run_as(app: &mut WordApp, name: &str, run: impl FnOnce(&mut WordApp) -> Result<Value, String>) -> Result<Value, String> {
    let name = crate::ask_agent::clean_agent_name(name).ok_or("`agent` is the agent's name")?;
    sync(app);
    let editor = app.session.editor.replace(wordcraft_doc::agents::author(Some(&name)));
    let r = run(app);
    app.session.editor = editor;
    r
}

/// The parts of `ui.askAgent` about ranges and comments; `None` for the others.
/// `release`: let other agents change request `id`'s text while it runs. `goTo`: select the text
/// request `id` covers now. `anchorComment`: anchor its closing comment now (the control channel
/// asks it right after the answer, so the agent learns the comment's id).
pub fn command(app: &mut WordApp, p: &Value) -> Option<Result<Value, String>> {
    let id = |k: &str| p.get(k).map(|v| v.as_u64().ok_or(format!("`{k}` is a request id")));
    if let Some(id) = id("release") {
        return Some(id.map(|id| {
            let t = app.ask_agent.queue.tracked.entry(id).or_default();
            t.released = true;
            let released = t.lock.is_some_and(|l| app.session.locks.set_owner(l, None));
            json!({"released": released})
        }));
    }
    if let Some(id) = id("goTo") {
        return Some(id.and_then(|id| {
            let r = app.ask_agent.hub.as_ref().and_then(|h| h.request(id)).ok_or_else(|| format!("no Ask Agent request {id}"))?;
            let (a, b) = range(app, &r).ok_or_else(|| format!("Ask Agent request {id} has no place in the document"))?;
            app.session.sel = wordcraft_engine::Selection { anchor: a.clone(), focus: b.clone() };
            app.canvas.scroll_to_caret = true;
            app.canvas.want_focus = true;
            Ok(json!({"anchor": wordcraft_engine::cmd::pos_json(&a), "focus": wordcraft_engine::cmd::pos_json(&b)}))
        }));
    }
    if let Some(id) = id("anchorComment") {
        return Some(id.and_then(|id| {
            let c = anchor_comment(app, id)?;
            release_lock(app, id);
            Ok(json!({"comment": c}))
        }));
    }
    None
}

/// `ui.inspect`'s `askAgent`, with each request's author, colour, range (and whether it is held,
/// and by whom) and closing comment.
pub fn inspect(app: &WordApp) -> Value {
    let mut v = app.ask_agent.inspect();
    let Some(hub) = &app.ask_agent.hub else { return v };
    let requests = hub.requests();
    if let Some(list) = v.get_mut("requests").and_then(Value::as_array_mut) {
        for item in list.iter_mut() {
            let Some(r) = item.get("id").and_then(Value::as_u64).and_then(|id| requests.iter().find(|r| r.id == id)) else { continue };
            let t = app.ask_agent.queue.tracked.get(&r.id);
            let lock = t.and_then(|t| t.lock).and_then(|l| app.session.locks.get(l));
            let c = color(r);
            item["author"] = json!(r.agent.as_ref().map(|_| r.author()));
            item["color"] = json!(r.agent.as_ref().map(|_| format!("{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())));
            item["range"] = json!(lock.map(|l| json!({
                "start": wordcraft_engine::cmd::pos_json(&l.start),
                "end": wordcraft_engine::cmd::pos_json(&l.end),
                "heldBy": l.owner,
            })));
            item["comment"] = json!(t.and_then(|t| t.comment));
        }
    }
    v
}

/// The text of each request an agent is working on: tinted in its colour, underlined, with its
/// name above the first line (and "holds it" while other agents can't change it).
pub fn paint(app: &WordApp, painter: &egui::Painter, layout: &wordcraft_layout::DocLayout, rects: &[Rect], scale: f32) {
    let Some(hub) = &app.ask_agent.hub else { return };
    for r in hub.requests().iter().filter(|r| r.state == State::Working) {
        let Some(t) = app.ask_agent.queue.tracked.get(&r.id) else { continue };
        let Some(l) = t.lock.and_then(|l| app.session.locks.get(l)) else { continue };
        let c = color(r);
        let mut first: Option<Rect> = None;
        for (pi, sr) in layout.selection_rects(&app.session.doc, &l.start, &l.end, app.session.page_hint).into_iter().take(2000) {
            let Some(pr) = rects.get(pi) else { continue };
            let s = layout.pages.get(pi).map_or(scale, |page| crate::canvas::page_screen_scale(*pr, page, scale));
            let rr = Rect::from_min_size(pos2(pr.min.x + sr.x * s, pr.min.y + sr.y * s), vec2(sr.w * s, sr.h * s));
            if !rr.is_finite() {
                continue;
            }
            painter.rect_filled(rr, 0.0, c.gamma_multiply(0.12));
            painter.hline(rr.x_range(), rr.max.y - 0.5, egui::Stroke::new(1.5, c.gamma_multiply(0.8)));
            if first.is_none_or(|f| (rr.min.y, rr.min.x) < (f.min.y, f.min.x)) {
                first = Some(rr);
            }
        }
        let Some(f) = first else { continue };
        let name = r.agent.clone().unwrap_or_else(|| tl!("Agent").to_string());
        let label = if l.owner.is_some() { crate::i18n::fmt(tl!("{agent} · holds this text"), &[("agent", &name)]) } else { name };
        let galley = painter.layout_no_wrap(label, crate::theme::semibold(10.5), Color32::WHITE);
        let size = galley.size() + vec2(8.0, 2.0);
        let tag = Rect::from_min_size(pos2(f.min.x, f.min.y - size.y - 1.0), size);
        painter.rect_filled(tag, egui::CornerRadius::same(3), c);
        painter.galley(tag.min + vec2(4.0, 1.0), galley, Color32::WHITE);
    }
}

/// What a click in the list asks for.
enum Act {
    Cancel(u64),
    Dismiss(u64),
    Release(u64),
    GoTo(u64),
}

/// A request's state in a word.
fn state_word(s: State) -> &'static str {
    match s {
        State::Waiting => tl!("Waiting"),
        State::Working => tl!("Working"),
        State::Answered => tl!("Answered"),
        State::Cancelled => tl!("Cancelled"),
    }
}

/// The requests the bubble doesn't show, in the canvas's bottom-right corner: every open one
/// (oldest first) with its agent, colour, instruction and state, then the newest closed ones.
pub fn show_list(app: &mut WordApp, ctx: &egui::Context, viewport: Rect, in_bubble: Option<u64>) {
    let Some(hub) = app.ask_agent.hub.clone() else { return };
    let all: Vec<Request> = hub.requests().into_iter().filter(|r| Some(r.id) != in_bubble).collect();
    let open: Vec<&Request> = all.iter().filter(|r| r.state.open()).collect();
    let closed: Vec<&Request> = {
        let mut c: Vec<&Request> = all.iter().filter(|r| !r.state.open()).rev().take(SHOWN).collect();
        c.reverse();
        c
    };
    if open.is_empty() && closed.is_empty() {
        return;
    }
    let present = hub.agent_present();
    let animate = app.animations_on();
    let held: Vec<u64> = open
        .iter()
        .filter(|r| {
            app.ask_agent.queue.tracked.get(&r.id).and_then(|t| t.lock).and_then(|l| app.session.locks.get(l)).is_some_and(|l| l.owner.is_some())
        })
        .map(|r| r.id)
        .collect();
    let t = Tokens::get(ctx);
    let mut acts: Vec<Act> = Vec::new();
    let mut close = false;
    egui::Area::new(egui::Id::new("wc_ask_agent_requests"))
        .order(Order::Foreground)
        .pivot(Align2::RIGHT_BOTTOM)
        .fixed_pos(viewport.right_bottom() - vec2(16.0, 16.0))
        .show(ctx, |ui| {
            card(&t).show(ui, |ui| {
                ui.set_width(WIDTH);
                close = header(ui, &t);
                if open.len() > 1 {
                    let n = open.len().to_string();
                    ui.label(egui::RichText::new(crate::i18n::fmt(tl!("{count} requests open"), &[("count", &n)])).small().color(t.text_dim));
                }
                egui::ScrollArea::vertical().id_salt("wc_ask_agent_list").max_height((viewport.height() * 0.6).max(160.0)).show(ui, |ui| {
                    for r in open.iter().chain(closed.iter()) {
                        ui.separator();
                        ui.horizontal(|ui| {
                            let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.5, color(r));
                            let who = r.agent.clone().unwrap_or_else(|| tl!("Not taken yet").to_string());
                            ui.label(egui::RichText::new(who).font(crate::theme::semibold(12.0)).color(t.text));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(egui::RichText::new(state_word(r.state)).small().color(t.text_dim));
                            });
                        });
                        ui.add(egui::Label::new(egui::RichText::new(shown_title(r)).small().color(t.text_dim)).truncate())
                            .on_hover_text(&r.instruction);
                        status(ui, r, present, animate, None);
                        if held.contains(&r.id) {
                            ui.label(egui::RichText::new(tl!("Other agents can't change this text until it is done.")).small().weak());
                        }
                        ui.horizontal(|ui| {
                            if r.state.open() {
                                if ui.small_button(tl!("Cancel")).clicked() {
                                    acts.push(Act::Cancel(r.id));
                                }
                                if held.contains(&r.id)
                                    && ui
                                        .small_button(tl!("Release"))
                                        .on_hover_text(tl!("Let other agents change this text while the request runs"))
                                        .clicked()
                                {
                                    acts.push(Act::Release(r.id));
                                }
                            } else if ui.small_button(tl!("Dismiss")).clicked() {
                                acts.push(Act::Dismiss(r.id));
                            }
                            if r.state.open() && ui.small_button(tl!("Go to text")).clicked() {
                                acts.push(Act::GoTo(r.id));
                            }
                        });
                    }
                });
            });
        });
    // Closing the card puts away the answered and cancelled requests; open ones stay.
    if close {
        acts.extend(closed.iter().map(|r| Act::Dismiss(r.id)));
    }
    for act in acts {
        match act {
            Act::Cancel(id) => {
                let _ = hub.cancel(id);
            }
            Act::Dismiss(id) => hub.dismiss(id),
            Act::Release(id) => {
                let _ = command(app, &json!({"release": id}));
            }
            Act::GoTo(id) => {
                let _ = command(app, &json!({"goTo": id}));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask_agent::AgentHub;
    use std::time::Duration;

    fn app(hub: &AgentHub, text: &str) -> WordApp {
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), Default::default()).with_agent_hub(hub.clone());
        a.run("document.setText", json!({"text": text})).unwrap();
        a
    }

    /// `engine.execute` through the control channel, as an agent's bridge sends it.
    fn exec(a: &mut WordApp, params: Value) -> Value {
        let ctx = egui::Context::default();
        let (req, _) = crate::ControlRequest::new("engine.execute", params);
        match crate::control::handle(a, &ctx, &req) {
            crate::control::Outcome::Done(v) => v,
            crate::control::Outcome::Screenshot { .. } => Value::Null,
        }
    }

    fn text(a: &WordApp) -> String {
        a.session.doc.plain_text(wordcraft_doc::StoryRef::Body)
    }

    /// Two agents, two requests on overlapping text: the second agent's edit inside the first
    /// one's text is refused with a clear error; each agent's edits are tracked under its own name.
    #[test]
    fn two_agents_cannot_change_the_same_text_at_once() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "Kilns fire clay slowly over many hours.");
        a.run("select.text", json!({"text": "clay slowly"})).unwrap();
        let first = a.run("ui.askAgent", json!({"instruction": "use a better phrase"})).unwrap()["id"].as_u64().unwrap();
        a.run("select.text", json!({"text": "slowly over many hours"})).unwrap();
        let second = a.run("ui.askAgent", json!({"instruction": "shorten"})).unwrap()["id"].as_u64().unwrap();
        assert_eq!(hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).map(|r| r.id), Some(first));
        assert_eq!(hub.listen_as(Duration::from_millis(10), Some("beta"), || true).map(|r| r.id), Some(second));
        // Alpha edits its text.
        assert_eq!(exec(&mut a, json!({"command": "select.text", "params": {"text": "clay"}, "request": first}))["ok"], true);
        assert_eq!(exec(&mut a, json!({"command": "text.insert", "params": {"text": "stoneware"}, "request": first}))["ok"], true);
        // Beta reaches into alpha's text ("slowly" is in both): refused, the document unchanged.
        let before = text(&a);
        exec(&mut a, json!({"command": "select.text", "params": {"text": "slowly"}, "request": second}));
        let r = exec(&mut a, json!({"command": "text.insert", "params": {"text": "gently"}, "request": second}));
        let e = r["error"].as_str().unwrap_or("");
        assert!(r["ok"] == false && e.contains("Agent (alpha) is working on"), "{r}");
        assert_eq!(text(&a), before);
        // Beta changes the part only it covers: fine.
        exec(&mut a, json!({"command": "select.text", "params": {"text": "many hours"}, "request": second}));
        assert_eq!(exec(&mut a, json!({"command": "text.insert", "params": {"text": "a day"}, "request": second}))["ok"], true);
        let authors: std::collections::BTreeSet<String> = a.session.doc.revisions.iter().map(|r| r.author.clone()).collect();
        assert_eq!(authors.into_iter().collect::<Vec<_>>(), ["Agent (alpha)", "Agent (beta)"]);
        // `ui.inspect` says who holds what.
        let ctx = egui::Context::default();
        let v = crate::control::inspect(&mut a, &ctx);
        let reqs = v["askAgent"]["requests"].as_array().unwrap();
        assert_eq!(reqs[0]["range"]["heldBy"], "Agent (alpha)");
        assert_eq!(reqs[1]["author"], "Agent (beta)");
        assert!(reqs[0]["color"].as_str().is_some_and(|c| c.len() == 6));
        // The person releases alpha's text: beta may change it now. The person was never refused.
        a.run("ui.askAgent", json!({"release": first})).unwrap();
        exec(&mut a, json!({"command": "select.text", "params": {"text": "slowly"}, "request": second}));
        assert_eq!(exec(&mut a, json!({"command": "text.insert", "params": {"text": "gently"}, "request": second}))["ok"], true);
        // An agent outside any request is held back too.
        a.run("ui.askAgent", json!({"cancel": second})).unwrap();
        let third = {
            a.run("select.text", json!({"text": "Kilns"})).unwrap();
            a.run("ui.askAgent", json!({"instruction": "capitalize"})).unwrap()["id"].as_u64().unwrap()
        };
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        a.run("select.text", json!({"text": "Kilns"})).unwrap();
        let r = exec(&mut a, json!({"command": "text.insert", "params": {"text": "Ovens"}, "agent": "gamma"}));
        assert!(r["error"].as_str().unwrap_or("").contains("Agent (alpha)"), "{r}");
        let _ = third;
    }

    /// The closing comment lands on the text the agent changed, by the agent; the person's
    /// selection, name and Comments pane stay as they were; the range is let go.
    #[test]
    fn the_closing_comment_is_anchored_on_the_changed_text() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "Kilns fire clay. Glazes melt.");
        a.session.author = "Ana Writer".into();
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "a better word"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        exec(&mut a, json!({"command": "select.text", "params": {"text": "clay"}, "request": id}));
        exec(&mut a, json!({"command": "text.insert", "params": {"text": "stoneware"}, "request": id}));
        // The person moves on meanwhile.
        a.run("select.text", json!({"text": "Glazes"})).unwrap();
        hub.set_comment(id, "Replaced clay with stoneware.").unwrap();
        hub.reply(id, "Done.", true).unwrap();
        let c = a.run("ui.askAgent", json!({"anchorComment": id})).unwrap()["comment"].as_u64().unwrap();
        let comment = a.session.doc.comments.get(&(c as u32)).unwrap();
        assert_eq!(comment.author, "Agent (alpha)");
        assert_eq!(a.session.doc.plain_text(wordcraft_doc::StoryRef::Part(comment.part)), "Replaced clay with stoneware.");
        // Anchored around the agent's change: its insertion and the word it deleted.
        let p = a.session.doc.para(wordcraft_doc::StoryRef::Body, &wordcraft_doc::Path::top(0)).unwrap();
        let (mut from, mut to) = (None, None);
        for off in p.object_offsets() {
            match p.object_at(off) {
                Some(wordcraft_doc::InlineObject::CommentStart { id }) if u64::from(*id) == c => {
                    from = Some(off + wordcraft_doc::para::OBJ.len_utf8())
                }
                Some(wordcraft_doc::InlineObject::CommentEnd { id }) if u64::from(*id) == c => to = Some(off),
                _ => {}
            }
        }
        assert_eq!(p.text.get(from.unwrap()..to.unwrap()), Some("stonewareclay"));
        assert_eq!(a.session.selected_text(), "Glazes", "the person's selection is kept");
        assert_eq!(a.session.author, "Ana Writer");
        assert!(!a.session.view.comments_pane);
        assert!(a.session.locks.is_empty(), "the range is let go");
        // Once only, and the next frame's sync doesn't add another.
        assert_eq!(a.run("ui.askAgent", json!({"anchorComment": id})).unwrap()["comment"], json!(c));
        sync(&mut a);
        assert_eq!(a.session.doc.comments.len(), 1);
        // A request answered without a comment adds none; a cancelled one's comment is dropped.
        a.run("select.text", json!({"text": "melt"})).unwrap();
        let quiet = a.run("ui.askAgent", json!({"instruction": "nothing to say"})).unwrap()["id"].as_u64().unwrap();
        hub.listen(Duration::from_millis(10), || true).unwrap();
        hub.reply(quiet, "Fine as it is.", true).unwrap();
        sync(&mut a);
        assert_eq!(a.session.doc.comments.len(), 1);
        assert!(hub.set_comment(quiet, "late").unwrap_err().contains("already answered"));
        assert!(hub.set_comment(999, "x").is_err());
        assert!(hub.set_comment(quiet, "   ").is_err());
    }

    #[test]
    fn table_closing_comments_are_rejected_until_cell_ranges_are_supported() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "");
        a.run("insert.table", json!({"rows": 1, "cols": 1})).unwrap();
        a.run("text.insert", json!({"text": "clay"})).unwrap();
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "rewrite"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        assert_eq!(exec(&mut a, json!({"command": "text.insert", "params": {"text": "stoneware"}, "request": id}))["ok"], true);
        hub.set_comment(id, "Reworded.").unwrap();
        hub.reply(id, "Done.", true).unwrap();
        assert!(a.run("ui.askAgent", json!({"anchorComment": id})).unwrap_err().contains("cell-level range tracking"));
        sync(&mut a);
        assert!(a.session.doc.comments.is_empty());
        assert_eq!(hub.request(id).unwrap().reply, "Done.");
    }

    #[test]
    fn releasing_contention_keeps_the_request_attached_to_its_edge_edits() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "Kilns fire clay.");
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "rewrite"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        a.run("ui.askAgent", json!({"release": id})).unwrap();
        a.session.sel.focus = a.session.sel.anchor.clone();
        assert_eq!(exec(&mut a, json!({"command": "text.insert", "params": {"text": "stoneware "}, "request": id}))["ok"], true);
        let (start, end) = range(&a, &hub.request(id).unwrap()).unwrap();
        assert_eq!(a.session.doc.copy_range(&start, &end).plain_text(), "stoneware clay");
        assert!(a.session.locks.list().iter().all(|l| l.owner.is_none()));
    }

    #[test]
    fn request_undo_cannot_change_text_outside_its_scope() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "Kilns fire clay. Glazes melt.");
        a.run("select.text", json!({"text": "Glazes"})).unwrap();
        a.run("text.insert", json!({"text": "Coatings"})).unwrap();
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "rewrite"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        let before = a.session.doc.clone();
        let r = exec(&mut a, json!({"command": "edit.undo", "request": id}));
        assert!(r["error"].as_str().unwrap().contains("outside the requested range"));
        assert_eq!(a.session.doc, before);
        a.run("edit.undo", json!({})).unwrap();
        assert_eq!(text(&a), "Kilns fire clay. Glazes melt.", "refused agent Undo must preserve the person's Undo step");
    }

    #[test]
    fn request_edits_outside_the_comment_anchor_are_refused() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "Kilns fire clay. Glazes melt.");
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "rewrite"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        exec(&mut a, json!({"command": "select.text", "params": {"text": "Glazes"}, "request": id}));
        let before = a.session.doc.clone();
        let r = exec(&mut a, json!({"command": "text.insert", "params": {"text": "Coatings"}, "request": id}));
        assert!(r["error"].as_str().unwrap().contains("outside the requested range"));
        assert_eq!(a.session.doc, before);
        hub.set_comment(id, "No edit.").unwrap();
        hub.reply(id, "Done.", true).unwrap();
        sync(&mut a);
        assert!(a.session.doc.comments.is_empty());
    }

    #[test]
    fn replacing_the_document_cancels_its_old_requests() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "Old text.");
        a.run("select.text", json!({"text": "Old"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "rewrite"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        let before = a.session.doc.clone();
        let dirty = a.session.dirty;
        let document_id = a.session.document_id();
        let r = exec(&mut a, json!({"command": "file.new", "request": id}));
        assert_eq!(r["ok"], false);
        assert_eq!(a.session.doc, before);
        assert_eq!(a.session.dirty, dirty);
        assert_eq!(a.session.document_id(), document_id);
        sync(&mut a);
        assert_eq!(hub.request(id).unwrap().state, State::Working);
        // Model the person confirming New after the normal unsaved-document prompt.
        a.session.set_document(wordcraft_doc::Document::new());
        sync(&mut a);
        assert_eq!(hub.request(id).unwrap().state, State::Cancelled);
        assert!(range(&a, &hub.request(id).unwrap()).is_none());
    }

    #[test]
    fn replies_and_selection_only_requests_do_not_add_document_comments() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "Kilns fire clay.");
        a.run("select.text", json!({"text": "clay"})).unwrap();
        let id = a.run("ui.askAgent", json!({"instruction": "explain"})).unwrap()["id"].as_u64().unwrap();
        hub.listen_as(Duration::from_millis(10), Some("alpha"), || true).unwrap();
        exec(&mut a, json!({"command": "select.text", "params": {"text": "clay"}, "request": id}));
        // A person edits while the agent only selects: this is not the agent's work.
        a.run("format.bold", json!({})).unwrap();
        hub.set_comment(id, "This is clay.").unwrap();
        hub.reply(id, "Explained.", true).unwrap();
        assert!(a.run("ui.askAgent", json!({"anchorComment": id})).unwrap()["comment"].is_null());
        sync(&mut a);
        assert!(a.session.doc.comments.is_empty());
    }

    /// Requests no agent has taken are followed but hold nobody back; the list shows every open
    /// request, whatever their number.
    #[test]
    fn waiting_requests_hold_nothing_and_all_open_requests_are_listed() {
        let hub = AgentHub::new(7985, None);
        let mut a = app(&hub, "One two three four five six.");
        for w in ["One", "two", "three", "four", "five"] {
            a.run("select.text", json!({"text": w})).unwrap();
            a.run("ui.askAgent", json!({"instruction": format!("change {w}")})).unwrap();
        }
        sync(&mut a);
        assert_eq!(a.session.locks.list().len(), 5);
        assert!(a.session.locks.list().iter().all(|l| l.owner.is_none()), "no agent has them yet");
        // Typing before them moves them all.
        a.run("caret.docStart", json!({})).unwrap();
        a.run("text.insert", json!({"text": "Say: "})).unwrap();
        let held: Vec<String> = a.session.locks.list().iter().map(|l| a.session.doc.copy_range(&l.start, &l.end).plain_text()).collect();
        assert_eq!(held, ["One", "two", "three", "four", "five"]);
        // Headless window: the corner card lists all five.
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1440.0, 900.0)).build_ui_state(
            |ui, app: &mut WordApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            a,
        );
        hub.listen_as(Duration::from_millis(1), Some("alpha"), || true).unwrap();
        for _ in 0..6 {
            h.step();
        }
        use egui_kittest::kittest::Queryable;
        for w in ["One", "two", "three", "four", "five"] {
            assert!(h.query_by_label_contains(&format!("change {w}")).is_some(), "{w} is listed");
        }
        assert!(h.query_by_label_contains("5 requests open").is_some());
        assert!(h.query_by_label("alpha").is_some(), "the agent that took one is named");
        assert!(h.query_by_label_contains("Sent to alpha").is_none(), "waiting requests are not routed to the last listener");
        // Go to text selects what a request covers now.
        let first = hub.requests()[0].id;
        h.state_mut().run("ui.askAgent", json!({"goTo": first})).unwrap();
        assert_eq!(h.state().session.selected_text(), "One");
    }
}
