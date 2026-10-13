//! Reviewers: who made which tracked changes and comments, and one reviewer's changes at a time.
//! Word's Review › Show Markup › Specific People (show or hide each reviewer's marks) and Accept /
//! Reject › All Changes Shown, plus accepting or rejecting everything one reviewer did. With
//! several agents editing one document (Ask Agent), each agent is a reviewer, `Agent (<name>)`.

use std::collections::{BTreeMap, BTreeSet};

use super::sel_result;
use serde_json::{Value, json};
use wordcraft_doc::{Document, StoryRef};

use crate::{CmdError, CmdResult, CommandSpec, Session, p};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("review.reviewers", "Specific People", "Review › Tracking › Show Markup", reviewers)
            .params(
                r#"{"author"?: string, "show"?: bool (omit to toggle), "only"?: string (show this reviewer alone), "all"?: bool (show everyone, or no one)} → {reviewers: [{author, agent, color, shown, changes, comments}], allShown}"#,
            )
            .pure(),
    ]
}

/// The tracked changes still in the document, by revision (index into `Document::revisions`).
fn live_revisions(doc: &Document) -> BTreeSet<u32> {
    let mut live = BTreeSet::new();
    let stories = std::iter::once(StoryRef::Body).chain(doc.parts.keys().map(|k| StoryRef::Part(*k)));
    for st in stories {
        for path in doc.para_paths(st) {
            let Some(para) = doc.para(st, &path) else { continue };
            let marks = para.runs.iter().map(|r| &r.props).chain(std::iter::once(&para.mark));
            for c in marks {
                live.extend(c.ins);
                live.extend(c.del);
                live.extend(c.fmt_change.as_ref().map(|f| f.rev));
            }
            live.extend(para.props.fmt_change.as_ref().map(|f| f.rev));
            live.extend(para.props.num_change.as_ref().map(|f| f.rev));
            live.extend(para.section.as_ref().and_then(|sec| sec.fmt_change.as_ref()).map(|f| f.rev));
            if let Some((tp, row, cell)) = path.cell()
                && let Some(table) = doc.table(st, &tp)
            {
                live.extend(table.props.fmt_change.as_ref().map(|f| f.rev));
                if let Some(row) = table.rows.get(row) {
                    live.extend(row.props.fmt_change.as_ref().map(|f| f.rev));
                    live.extend(row.cells.get(cell).and_then(|c| c.props.fmt_change.as_ref()).map(|f| f.rev));
                }
            }
        }
    }
    live.extend(doc.last_section.fmt_change.as_ref().map(|f| f.rev));
    live
}

/// Everyone with a tracked change or a comment in the document, in the order Word colours them
/// (first the authors of revisions, then those with only comments), with their colour and counts.
pub fn list(s: &Session) -> Vec<Value> {
    let doc = &s.doc;
    let live = live_revisions(doc);
    let mut order: Vec<String> = Vec::new();
    let mut changes: BTreeMap<String, usize> = BTreeMap::new();
    for (i, r) in doc.revisions.iter().enumerate() {
        if !order.contains(&r.author) {
            order.push(r.author.clone());
        }
        if u32::try_from(i).is_ok_and(|i| live.contains(&i)) {
            *changes.entry(r.author.clone()).or_default() += 1;
        }
    }
    // A revision author's colour is its place among revision authors; one with comments only,
    // its place among comment authors (`wordcraft_layout::balloons`).
    let revision_authors = order.len();
    let mut comment_order: Vec<&str> = Vec::new();
    let mut comments: BTreeMap<String, usize> = BTreeMap::new();
    for c in doc.comments.values() {
        if !comment_order.contains(&c.author.as_str()) {
            comment_order.push(&c.author);
        }
        *comments.entry(c.author.clone()).or_default() += 1;
        if !order.contains(&c.author) {
            order.push(c.author.clone());
        }
    }
    order
        .iter()
        .enumerate()
        .map(|(i, author)| {
            let index = if i < revision_authors { i } else { comment_order.iter().position(|a| a == author).unwrap_or(0) };
            let c = wordcraft_layout::display::author_color(u32::try_from(index).unwrap_or(0), author);
            json!({
                "author": author,
                "agent": wordcraft_doc::agents::is_agent(author),
                "color": format!("{:02X}{:02X}{:02X}", c.0, c.1, c.2),
                "shown": !s.view.hidden_reviewers.contains(author),
                "changes": changes.get(author).copied().unwrap_or(0),
                "comments": comments.get(author).copied().unwrap_or(0),
            })
        })
        .collect()
}

/// Most reviewers hidden at once (names come from files and agents).
const MAX_HIDDEN: usize = 256;

/// `review.reviewers`: list the reviewers; `{author, show?}` shows, hides or toggles one;
/// `{only}` shows that one alone; `{all}` shows everyone (or no one).
fn reviewers(s: &mut Session, v: &Value) -> CmdResult {
    let names: Vec<String> = list(s).iter().filter_map(|r| r.get("author").and_then(Value::as_str).map(str::to_string)).collect();
    let before = s.view.hidden_reviewers.clone();
    if let Some(all) = p::bool(v, "all") {
        s.view.hidden_reviewers = if all { Vec::new() } else { names.clone() };
    }
    if let Some(only) = p::str(v, "only") {
        s.view.hidden_reviewers = names.iter().filter(|n| n.as_str() != only).cloned().collect();
    }
    if let Some(author) = p::str(v, "author") {
        let hidden = s.view.hidden_reviewers.iter().any(|h| h == author);
        let show = p::bool(v, "show").unwrap_or(hidden);
        s.view.hidden_reviewers.retain(|h| h != author);
        if !show {
            s.view.hidden_reviewers.push(author.to_string());
        }
    } else if v.get("show").is_some() {
        return Err(CmdError::Params("`show` goes with `author`, the reviewer to show or hide".into()));
    }
    s.view.hidden_reviewers.truncate(MAX_HIDDEN);
    if s.view.hidden_reviewers != before {
        // Hidden reviewers' deletions leave the layout.
        s.relayout();
    }
    let list = list(s);
    let all_shown = s.view.hidden_reviewers.is_empty();
    Ok(json!({"reviewers": list, "allShown": all_shown}))
}

/// `review.acceptAll` / `review.rejectAll` with `{author}` (that reviewer's changes) or
/// `{shown: true}` (the changes of the reviewers shown: All Changes Shown); `None` when neither is
/// given (every change).
pub fn resolve_filtered(s: &mut Session, v: &Value, accept: bool) -> Option<CmdResult> {
    if v.get("author").is_some_and(|author| !author.is_string()) || v.get("shown").is_some_and(|shown| !shown.is_boolean()) {
        return Some(Err(CmdError::Params(
            "`author` is a reviewer name and `shown` is a boolean; invalid filters never resolve all revisions".into(),
        )));
    }
    if let Some(author) = p::str(v, "author") {
        let author = author.to_string();
        return Some(resolve_by(s, accept, &|a| a == author));
    }
    if p::bool(v, "shown") == Some(true) {
        let hidden = s.view.hidden_reviewers.clone();
        return Some(resolve_by(s, accept, &|a| !hidden.iter().any(|h| h == a)));
    }
    None
}

/// Resolve the chosen authors through the revision-id resolver shared with Ask Agent review.
fn resolve_by(s: &mut Session, accept: bool, pick: &dyn Fn(&str) -> bool) -> CmdResult {
    let live = live_revisions(&s.doc);
    let revisions: Vec<u32> = s
        .doc
        .revisions
        .iter()
        .enumerate()
        .filter(|(_, r)| pick(&r.author))
        .filter_map(|(i, _)| u32::try_from(i).ok())
        .filter(|i| live.contains(i))
        .collect();
    let resolved = revisions.len();
    let mut result = if revisions.is_empty() {
        sel_result(s)?
    } else {
        s.join_next_undo();
        s.run(if accept { "review.accept" } else { "review.reject" }, &json!({"revisions": revisions}))?
    };
    if let Some(result) = result.as_object_mut() {
        result.insert("resolved".into(), json!(resolved));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::Session;
    use serde_json::{Value, json};
    use wordcraft_doc::{Document, StoryRef};

    fn run(s: &mut Session, id: &str, v: Value) -> Value {
        s.run(id, &v).unwrap_or_else(|e| panic!("{id}: {e}"))
    }

    /// Two agents and a person edit with tracking on.
    fn three_reviewers() -> Session {
        let mut s = Session::new(Document::new());
        run(&mut s, "document.setText", json!({"text": "Kilns fire clay. Glazes melt."}));
        run(&mut s, "review.trackChanges", json!({"value": true}));
        for (who, word, new) in [("Agent (alpha)", "clay", "stoneware"), ("Agent (beta)", "melt", "flow"), ("Ana", "Kilns", "Ovens")] {
            s.author = who.into();
            run(&mut s, "select.text", json!({"text": word}));
            run(&mut s, "text.insert", json!({"text": new}));
        }
        s.author = "Ana".into();
        run(&mut s, "select.text", json!({"text": "Glazes"}));
        run(&mut s, "review.newComment", json!({"text": "check"}));
        s
    }

    fn text(s: &Session) -> String {
        s.doc.plain_text(StoryRef::Body)
    }

    #[test]
    fn reviewers_list_agents_with_their_own_colours() {
        let mut s = three_reviewers();
        let r = run(&mut s, "review.reviewers", json!({}));
        let list = r["reviewers"].as_array().unwrap();
        let names: Vec<&str> = list.iter().map(|x| x["author"].as_str().unwrap()).collect();
        assert_eq!(names, ["Agent (alpha)", "Agent (beta)", "Ana"]);
        assert_eq!(list[0]["changes"], 2, "a deletion and an insertion");
        assert_eq!((list[0]["agent"].as_bool(), list[2]["agent"].as_bool()), (Some(true), Some(false)));
        assert_eq!(list[2]["comments"], 1);
        // An agent's colour comes from its name, whatever the document.
        let alpha = wordcraft_layout::display::agent_color("alpha");
        assert_eq!(list[0]["color"], format!("{:02X}{:02X}{:02X}", alpha.0, alpha.1, alpha.2));
        assert_eq!(r["allShown"], true);
    }

    #[test]
    fn hiding_a_reviewer_hides_its_marks_and_its_deletions() {
        let mut s = three_reviewers();
        let doc = s.doc.clone();
        let r = run(&mut s, "review.reviewers", json!({"author": "Agent (beta)", "show": false}));
        assert_eq!(r["allShown"], false);
        assert_eq!(s.view.hidden_reviewers, ["Agent (beta)"]);
        // Beta's deleted word takes no room; alpha's stays struck through in the text.
        let laid: String = wordcraft_layout::hidden_revisions(&s.doc, &s.view.hidden_reviewers)
            .iter()
            .map(|r| s.doc.revisions[*r as usize].author.clone())
            .collect();
        assert!(laid.contains("beta") && !laid.contains("alpha"));
        // Toggle back; `only` shows one reviewer alone; `all` brings everyone back.
        run(&mut s, "review.reviewers", json!({"author": "Agent (beta)"}));
        assert!(s.view.hidden_reviewers.is_empty());
        run(&mut s, "review.reviewers", json!({"only": "Agent (alpha)"}));
        assert_eq!(s.view.hidden_reviewers, ["Agent (beta)", "Ana"]);
        run(&mut s, "review.reviewers", json!({"all": true}));
        assert!(s.view.hidden_reviewers.is_empty());
        assert!(s.run("review.reviewers", &json!({"show": true})).is_err(), "show needs an author");
        // A view setting: the document doesn't change.
        assert!(s.doc == doc);
    }

    #[test]
    fn malformed_author_filters_never_resolve_everyone() {
        let mut s = three_reviewers();
        let before = s.doc.clone();
        for params in [json!({"author": 7}), json!({"shown": "true"})] {
            assert!(s.run("review.acceptAll", &params).is_err());
            assert!(s.run("review.rejectAll", &params).is_err());
            assert_eq!(s.doc, before);
        }
    }

    #[test]
    fn author_filter_preserves_other_authors_paragraph_revision_records() {
        use wordcraft_doc::{
            Revision, RevisionKind,
            props::{NumChange, NumRef, ParaProps, PropChange},
        };
        for (author, accept) in [("Agent (beta)", true), ("Agent (alpha)", true), ("Agent (alpha)", false)] {
            let mut s = Session::new(Document::new());
            s.doc.revisions = ["Agent (alpha)", "Agent (beta)"]
                .into_iter()
                .map(|author| Revision { kind: RevisionKind::Format, author: author.into(), date: "2026-01-01T00:00:00Z".into(), move_name: None })
                .collect();
            let p = s.doc.para_mut(StoryRef::Body, &wordcraft_doc::Path::top(0)).unwrap();
            p.props.fmt_change = PropChange::boxed(0, ParaProps::default());
            p.props.num_change = Some(Box::new(NumChange { rev: 1, original: "1".into() }));
            p.props.numbering = Some(NumRef { num: 1, level: 0 });
            run(&mut s, if accept { "review.acceptAll" } else { "review.rejectAll" }, json!({"author": author}));
            let p = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(0)).unwrap();
            if author == "Agent (beta)" {
                assert!(p.props.fmt_change.is_some());
                assert!(p.props.num_change.is_none());
            } else {
                assert!(p.props.fmt_change.is_none());
                assert_eq!(p.props.num_change.as_ref().map(|n| n.rev), Some(1));
                assert_eq!(p.props.numbering, Some(NumRef { num: 1, level: 0 }));
            }
        }
    }

    #[test]
    fn author_filter_resolves_revisions_in_other_stories() {
        let mut s = Session::new(Document::new());
        let part = s.doc.add_part(
            wordcraft_doc::PartKind::Header,
            vec![wordcraft_doc::para_block(wordcraft_doc::Paragraph::with_text("Header", Default::default()))],
        );
        s.sel = crate::Selection::caret(wordcraft_doc::Pos { story: StoryRef::Part(part), path: wordcraft_doc::Path::top(0), off: 0 });
        s.author = "Agent (alpha)".into();
        s.force_tracking = true;
        run(&mut s, "text.insert", json!({"text": "New "}));
        s.force_tracking = false;
        let r = run(&mut s, "review.acceptAll", json!({"author": "Agent (alpha)"}));
        assert_eq!(r["resolved"], 1);
        let p = s.doc.para(StoryRef::Part(part), &wordcraft_doc::Path::top(0)).unwrap();
        assert!(p.runs.iter().all(|r| r.props.ins.is_none()));
    }

    #[test]
    fn accept_or_reject_one_reviewer_at_a_time() {
        let mut s = three_reviewers();
        assert_eq!(text(&s), "OvensKilns fire stonewareclay. Glazes flowmelt.");
        // Reject everything beta did: its word comes back, the others' changes stay.
        let r = run(&mut s, "review.rejectAll", json!({"author": "Agent (beta)"}));
        assert_eq!(r["resolved"], 2);
        assert_eq!(text(&s), "OvensKilns fire stonewareclay. Glazes melt.");
        // Accept alpha's.
        run(&mut s, "review.acceptAll", json!({"author": "Agent (alpha)"}));
        assert_eq!(text(&s), "OvensKilns fire stoneware. Glazes melt.");
        let left: Vec<String> =
            run(&mut s, "review.changes", json!({})).as_array().unwrap().iter().map(|c| c["author"].as_str().unwrap_or("").to_string()).collect();
        assert!(!left.is_empty() && left.iter().all(|a| a == "Ana"), "{left:?}");
        // All Changes Shown: Ana is hidden, so nothing of hers is accepted.
        run(&mut s, "review.reviewers", json!({"author": "Ana", "show": false}));
        let r = run(&mut s, "review.acceptAll", json!({"shown": true}));
        assert_eq!(r["resolved"], 0);
        assert_eq!(text(&s), "OvensKilns fire stoneware. Glazes melt.");
        // Undo takes the three steps back.
        run(&mut s, "edit.undo", json!({}));
        run(&mut s, "edit.undo", json!({}));
        run(&mut s, "edit.undo", json!({}));
        assert_eq!(text(&s), "OvensKilns fire stonewareclay. Glazes flowmelt.");
        // Plain Accept All still takes everything.
        run(&mut s, "review.acceptAll", json!({}));
        assert_eq!(text(&s), "Ovens fire stoneware. Glazes flow.");
    }
}
