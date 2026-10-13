//! Range locks: several agents on one document. While an agent works on a range for the person
//! (an Ask Agent request), the range is held for it. A command another editor runs
//! ([`Session::editor`]) that changes text inside the range is refused with an error that says who
//! holds it; the person (no editor) is never refused and keeps the last word: the window cancels
//! the request or releases the range.
//!
//! A held range follows the text through every edit, the holder's own, the person's and Undo's:
//! after each outermost mutating command [`after_command`] compares the stories that hold ranges
//! before and after (blocks by identity, then the changed paragraphs' text and formatting at both
//! ends) and moves the ranges. Text the holder writes at its range's edges joins the range, so the
//! range ends up covering what the agent changed: the place for its closing comment.
//!
//! Granularity: inside a table, a range is held cell by cell only as precisely as the table block
//! as a whole (an edit anywhere in a table that holds a range counts as inside it).

use wordcraft_doc::{Block, Blocks, InlineObject, Paragraph, Pos, StoryRef};

use crate::{CmdError, Session};

/// A range held for an editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeLock {
    pub id: u64,
    /// Who holds it: an agent's author name (`Agent (<name>)`). `None` holds nobody back: the range
    /// is only followed (a request no agent has taken yet, or one the person released).
    pub owner: Option<String>,
    pub start: Pos,
    pub end: Pos,
}

/// The held ranges of a session.
#[derive(Clone, Debug, Default)]
pub struct Locks {
    list: Vec<RangeLock>,
    next: u64,
}

/// Most ranges held at once (older ones are let go first).
pub const MAX_LOCKS: usize = 64;

impl Locks {
    pub fn list(&self) -> &[RangeLock] {
        &self.list
    }
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
    pub fn get(&self, id: u64) -> Option<&RangeLock> {
        self.list.iter().find(|l| l.id == id)
    }
    /// Let range `id` go; what it covered last.
    pub fn remove(&mut self, id: u64) -> Option<RangeLock> {
        let i = self.list.iter().position(|l| l.id == id)?;
        Some(self.list.remove(i))
    }
    /// Give range `id` to `owner` (`None`: it holds nobody back any more). False when it is gone.
    pub fn set_owner(&mut self, id: u64, owner: Option<&str>) -> bool {
        match self.list.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                l.owner = owner.map(str::to_string);
                true
            }
            None => false,
        }
    }
    pub fn clear(&mut self) {
        self.list.clear();
    }
    fn push(&mut self, owner: Option<&str>, start: Pos, end: Pos) -> u64 {
        self.next = self.next.saturating_add(1);
        let id = self.next;
        self.list.push(RangeLock { id, owner: owner.map(str::to_string), start, end });
        if self.list.len() > MAX_LOCKS {
            self.list.remove(0);
        }
        id
    }
}

impl Session {
    /// Hold the range from `a` to `b` (either order, clamped to the document) for `owner`; its id.
    pub fn lock_range(&mut self, owner: Option<&str>, a: &Pos, b: &Pos) -> u64 {
        let (a, b) = (self.doc.clamp(a), self.doc.clamp(b));
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        self.locks.push(owner, start, end)
    }
}

/// Commands that put another document in the session: the ranges held in the old one go.
pub fn replaces_document(id: &str) -> bool {
    matches!(id, "file.new" | "file.open" | "file.close" | "file.recover" | "file.newFromTemplate")
}

/// The stories that hold a range, as they are now (blocks shared, not copied): what
/// [`after_command`] compares the document with.
pub(crate) fn held_stories(s: &Session) -> Vec<(StoryRef, Blocks)> {
    let mut stories: Vec<StoryRef> = s.locks.list.iter().map(|l| l.start.story).collect();
    stories.sort_unstable();
    stories.dedup();
    stories.into_iter().filter_map(|st| Some((st, s.doc.story(st)?.clone()))).collect()
}

/// After the outermost command `id` ran, with the stories that hold ranges as they were before it
/// (`before`, [`held_stories`]): refuse an edit (`mutates`) that changed text inside a range held
/// by someone other than [`Session::editor`] (the person, `None`, is never refused); otherwise
/// move every range with what changed (Undo and Redo included).
pub(crate) fn after_command(s: &mut Session, before: &[(StoryRef, Blocks)], id: &str, mutates: bool) -> Result<(), CmdError> {
    if s.locks.is_empty() {
        return Ok(());
    }
    if replaces_document(id) {
        if s.editor.is_some() {
            return Err(CmdError::Disabled(format!(
                "{id} was not run: an agent cannot replace the document while Ask Agent requests hold text; finish the requests first"
            )));
        }
        s.locks.clear();
        return Ok(());
    }
    let empty = Vec::new();
    let changes: Vec<Change> = before.iter().filter_map(|(story, b)| change(*story, b, s.doc.story(*story).unwrap_or(&empty))).collect();
    if changes.is_empty() {
        return Ok(());
    }
    if mutates
        && let Some(editor) = s.editor.as_deref()
        && let Some((lock, _)) = s
            .locks
            .list
            .iter()
            .filter_map(|l| Some((l, l.owner.as_deref().filter(|o| *o != editor)?)))
            .find(|(l, _)| changes.iter().any(|c| c.story == l.start.story && c.overlaps(l)))
    {
        let owner = lock.owner.clone().unwrap_or_default();
        return Err(CmdError::Disabled(format!(
            "{id} refused: {owner} is working on this text. Wait, edit another range, or ask the person to release it."
        )));
    }
    let editor = s.editor.clone();
    let doc = &s.doc;
    // A person may remove a header, note or comment story. Its requests must not drift into
    // the body when clamp falls back to it; missing locks let the window cancel those requests.
    s.locks.list.retain(|lock| doc.story(lock.start.story).is_some());
    for l in &mut s.locks.list {
        let story = l.start.story;
        for c in changes.iter().filter(|c| c.story == story) {
            // The holder's text written right at its range's edges joins the range; anyone else's
            // stays outside it.
            let grows = editor.is_some() && editor == l.owner;
            let start = c.map(&l.start, if grows || !c.insertion() { Side::Start } else { Side::End });
            let end = c.map(&l.end, if grows || !c.insertion() { Side::End } else { Side::Start });
            l.start = doc.clamp(&start);
            l.end = doc.clamp(&end);
            if l.end < l.start {
                l.end = l.start.clone();
            }
        }
    }
    Ok(())
}

/// Where a position inside a change goes: to the start of what replaced it, or to its end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Start,
    End,
}

/// A place in a story at the top level: block index, then byte offset (`usize::MAX` = past the
/// end of the block).
type Key = (usize, usize);

/// What one command changed in one story: top-level blocks `start..old_end` became
/// `start..new_end`. Within them, `old_from..old_to` (before) became `new_from..new_to` (after):
/// the text and formatting outside those ends stayed. `inner` says more when the change stays in
/// one paragraph and lines up simply.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Change {
    story: StoryRef,
    start: usize,
    old_end: usize,
    new_end: usize,
    old_from: Key,
    old_to: Key,
    new_from: Key,
    new_to: Key,
    inner: Option<Inner>,
}

/// How a change inside one paragraph lines up (byte offsets in that paragraph).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Inner {
    /// The same characters: only their formatting (or objects) changed, in these spans.
    Same(Vec<(usize, usize)>),
    /// Characters only went in (comment marks around a range, text typed at two places): each kept
    /// character as (before, after, length), and the places text went in (before).
    Added(Vec<(usize, usize, usize)>, Vec<usize>),
    /// Characters only came out: each kept character as (before, after, length), and the spans
    /// taken out (before).
    Removed(Vec<(usize, usize, usize)>, Vec<(usize, usize)>),
}

/// What changed between two versions of a story, or `None` when nothing did.
fn change(story: StoryRef, before: &Blocks, after: &Blocks) -> Option<Change> {
    let same = |a: &std::sync::Arc<Block>, b: &std::sync::Arc<Block>| std::sync::Arc::ptr_eq(a, b) || a == b;
    let common = before.len().min(after.len());
    let prefix = before.iter().zip(after.iter()).take_while(|(a, b)| same(a, b)).count();
    if prefix == before.len() && prefix == after.len() {
        return None;
    }
    let suffix = before.iter().rev().zip(after.iter().rev()).take(common - prefix).take_while(|(a, b)| same(a, b)).count();
    let (start, old_end, new_end) = (prefix, before.len() - suffix, after.len() - suffix);
    fn para(blocks: &Blocks, i: usize) -> Option<&Paragraph> {
        match blocks.get(i).map(|b| b.as_ref()) {
            Some(Block::Para(p)) => Some(p),
            _ => None,
        }
    }
    let len = |blocks: &Blocks, i: usize| match blocks.get(i).map(|block| block.as_ref()) {
        Some(Block::Table(_)) => usize::MAX,
        Some(Block::Para(para)) => para.len(),
        None => 0,
    };
    // Unchanged characters at the region's two ends, counted on the first and the last paragraph
    // on each side.
    let (mut head, mut tail) = (0, 0);
    if old_end > start && new_end > start {
        // A paragraph whose own formatting changed (alignment, style…) changed as a whole.
        if let (Some(b), Some(a)) = (para(before, start), para(after, start))
            && b.props == a.props
        {
            head = common_units(&units(b), &units(a), false);
        }
        if let (Some(b), Some(a)) = (para(before, old_end - 1), para(after, new_end - 1))
            && b.props == a.props
        {
            tail = common_units(&units(b), &units(a), true);
        }
        // One paragraph on a side: its head and tail can't overlap.
        let cap = |n: usize, head: usize, tail: usize| tail.min(n.saturating_sub(head));
        if old_end - start == 1 {
            tail = cap(para(before, start).map_or(0, |p| units(p).len()), head, tail);
        }
        if new_end - start == 1 {
            tail = cap(para(after, start).map_or(0, |p| units(p).len()), head, tail);
        }
    }
    let bytes_head = |blocks: &Blocks, i: usize| para(blocks, i).map_or(0, |p| char_bytes(p, head, false));
    let bytes_tail = |blocks: &Blocks, i: usize| para(blocks, i).map_or(0, |p| char_bytes(p, tail, true));
    let (old_from, old_to) = if old_end > start {
        ((start, bytes_head(before, start)), (old_end - 1, len(before, old_end - 1).saturating_sub(bytes_tail(before, old_end - 1))))
    } else {
        ((start, 0), (start, 0))
    };
    let (new_from, new_to) = if new_end > start {
        ((start, bytes_head(after, start)), (new_end - 1, len(after, new_end - 1).saturating_sub(bytes_tail(after, new_end - 1))))
    } else {
        ((start, 0), (start, 0))
    };
    let inner = match (old_end - start, new_end - start, para(before, start), para(after, start)) {
        (1, 1, Some(b), Some(a)) if b.props == a.props => {
            let (ub, ua) = (units(b), units(a));
            let nb = ub.len().saturating_sub(head + tail);
            let na = ua.len().saturating_sub(head + tail);
            match (ub.get(head..head + nb), ua.get(head..head + na)) {
                (Some(rb), Some(ra)) => line_up(rb, ra, old_to.1),
                _ => None,
            }
        }
        _ => None,
    };
    Some(Change { story, start, old_end, new_end, old_from, old_to, new_from, new_to, inner })
}

/// Whether story edits stay inside a request's range (including insertion at either edge).
/// Tables use the same conservative whole-block scope as range locks.
pub fn changes_within(before: &wordcraft_doc::Document, after: &wordcraft_doc::Document, start: &Pos, end: &Pos) -> bool {
    let mut stories = vec![StoryRef::Body];
    stories.extend(before.parts.keys().chain(after.parts.keys()).map(|id| StoryRef::Part(*id)));
    stories.sort_unstable();
    stories.dedup();
    let empty = Vec::new();
    let bounds = (key(start, Side::Start), key(end, Side::End));
    stories.into_iter().all(|story| {
        let Some(c) = change(story, before.story(story).unwrap_or(&empty), after.story(story).unwrap_or(&empty)) else { return true };
        story == start.story && c.old_from >= bounds.0 && c.old_to <= bounds.1
    })
}

/// One character of a paragraph with what makes it look as it does: its byte offset, the
/// character, its formatting and, for an object's placeholder, the object.
type Unit<'a> = (usize, char, &'a wordcraft_doc::CharProps, Option<&'a InlineObject>);

/// Most characters lined up one by one inside a changed paragraph (longer changes are taken as a
/// whole).
const MAX_LINE_UP: usize = 100_000;

fn units(p: &Paragraph) -> Vec<Unit<'_>> {
    let mut out = Vec::with_capacity(p.text.len());
    let mut objects = p.objects.iter();
    for (r, props) in p.run_ranges() {
        let Some(text) = p.text.get(r.clone()) else { continue };
        for (i, c) in text.char_indices() {
            let obj = if c == wordcraft_doc::para::OBJ { objects.next() } else { None };
            out.push((r.start + i, c, props, obj));
        }
    }
    out
}

/// Two characters look the same: same character, formatting and object.
fn same_unit(x: &Unit<'_>, y: &Unit<'_>) -> bool {
    x.1 == y.1 && (std::ptr::eq(x.2, y.2) || x.2 == y.2) && x.3 == y.3
}

/// How many characters two paragraphs share at the start (or at the end, `from_end`), text and
/// formatting alike.
fn common_units(a: &[Unit<'_>], b: &[Unit<'_>], from_end: bool) -> usize {
    if from_end {
        a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| same_unit(x, y)).count()
    } else {
        a.iter().zip(b).take_while(|(x, y)| same_unit(x, y)).count()
    }
}

/// How the changed characters `b` (before, ending at byte `b_end`) and `a` (after) line up: the same characters reformatted, characters only added, or only taken out;
/// `None` when it's a mix.
fn line_up(b: &[Unit<'_>], a: &[Unit<'_>], b_end: usize) -> Option<Inner> {
    if b.len() > MAX_LINE_UP || a.len() > MAX_LINE_UP {
        return None;
    }
    let end_of = |u: &[Unit<'_>], i: usize, end: usize| u.get(i + 1).map_or(end, |n| n.0);
    if b.len() == a.len() && b.iter().zip(a).all(|(x, y)| x.1 == y.1) {
        let spans = b.iter().enumerate().zip(a).filter(|((_, x), y)| !same_unit(x, y)).map(|((i, x), _)| (x.0, end_of(b, i, b_end))).collect();
        return Some(Inner::Same(spans));
    }
    // Greedy: each character of the shorter side matched with the next equal one of the longer.
    let matched = |short: &[Unit<'_>], long: &[Unit<'_>]| -> Option<Vec<usize>> {
        let mut at = 0;
        let mut m = Vec::with_capacity(short.len());
        for s in short {
            at += long.get(at..)?.iter().position(|l| same_unit(s, l))?;
            m.push(at);
            at += 1;
        }
        Some(m)
    };
    if b.len() < a.len()
        && let Some(m) = matched(b, a)
    {
        let kept: Vec<(usize, usize, usize)> = m.iter().zip(b).filter_map(|(&j, x)| Some((x.0, a.get(j)?.0, x.1.len_utf8()))).collect();
        // Text went in before each kept character whose match skipped some, and at the end.
        let mut at = Vec::new();
        let mut next = 0;
        for (k, &j) in m.iter().enumerate() {
            if j > next {
                at.push(b.get(k).map_or(b_end, |x| x.0));
            }
            next = j + 1;
        }
        if next < a.len() {
            at.push(b_end);
        }
        return Some(Inner::Added(kept, at));
    }
    if a.len() < b.len()
        && let Some(m) = matched(a, b)
    {
        let kept: Vec<(usize, usize, usize)> = m.iter().zip(a).filter_map(|(&i, y)| Some((b.get(i)?.0, y.0, y.1.len_utf8()))).collect();
        let mut spans = Vec::new();
        let mut next = 0;
        for &i in m.iter().chain(std::iter::once(&b.len())) {
            if i > next {
                let from = b.get(next).map_or(b_end, |x| x.0);
                spans.push((from, b.get(i).map_or(b_end, |x| x.0)));
            }
            next = i + 1;
        }
        return Some(Inner::Removed(kept, spans));
    }
    None
}

/// The bytes the first (or last, `from_end`) `n` characters of `p` take.
fn char_bytes(p: &Paragraph, n: usize, from_end: bool) -> usize {
    if from_end { p.text.chars().rev().take(n).map(char::len_utf8).sum() } else { p.text.chars().take(n).map(char::len_utf8).sum() }
}

impl Change {
    /// Text went in, nothing came out (at one place, or at several in one paragraph).
    fn insertion(&self) -> bool {
        self.old_from == self.old_to || matches!(self.inner, Some(Inner::Added(..)))
    }

    /// Whether this change touches text inside `l` (both before the change). Text that went in
    /// right at the range's edge isn't inside it; nor is a change that only touches its edges.
    fn overlaps(&self, l: &RangeLock) -> bool {
        let (ls, le) = (key(&l.start, Side::Start), key(&l.end, Side::End));
        if ls >= le {
            return false;
        }
        let row = self.start;
        let point = |p: Key| ls < p && p < le;
        let span = |from: Key, to: Key| if from == to { point(from) } else { from < le && to > ls };
        match &self.inner {
            Some(Inner::Same(spans)) | Some(Inner::Removed(_, spans)) => spans.iter().any(|&(x, y)| span((row, x), (row, y))),
            Some(Inner::Added(_, at)) => at.iter().any(|&x| point((row, x))),
            None => span(self.old_from, self.old_to),
        }
    }

    /// Where `p` (before the change) is after it; a position inside the change goes to the start
    /// or the end of what replaced it (`side`). Positions inside tables that changed stay as they
    /// are (the caller clamps them).
    fn map(&self, p: &Pos, side: Side) -> Pos {
        let Some(&first) = p.path.0.first() else { return p.clone() };
        let i = first as usize;
        let at = |k: Key| Pos { story: p.story, ..Pos::body(k.0, k.1) };
        if p.path.0.len() > 1 {
            // Inside a table (or deeper): only the table's place among the blocks moves.
            if i < self.old_end {
                return p.clone();
            }
            let mut q = p.clone();
            if let Some(f) = q.path.0.first_mut() {
                *f = u32::try_from((i + self.new_end).saturating_sub(self.old_end)).unwrap_or(u32::MAX);
            }
            return q;
        }
        let k: Key = (i, p.off);
        if i == self.start
            && self.old_from <= k
            && k <= self.old_to
            && let Some(inner) = &self.inner
        {
            return at((i, inner.map(p.off, side, self.new_from.1, self.new_to.1)));
        }
        let empty = self.old_from == self.old_to;
        if k < self.old_from || (k == self.old_from && !empty) {
            return p.clone();
        }
        if k > self.old_to || (k == self.old_to && !empty) {
            if i >= self.old_end {
                return at(((i + self.new_end).saturating_sub(self.old_end), p.off));
            }
            // In the region's last paragraph, after the change: the same distance from its end.
            if self.new_end == self.start {
                return at(self.new_from);
            }
            let off = p.off.saturating_sub(self.old_to.1) + self.new_to.1;
            return at((self.new_end - 1, off));
        }
        at(if side == Side::Start { self.new_from } else { self.new_to })
    }
}

impl Inner {
    /// Where byte `off` (before, inside the change's span) goes; `new_from..new_to` is the span
    /// after.
    fn map(&self, off: usize, side: Side, new_from: usize, new_to: usize) -> usize {
        let kept = match self {
            Inner::Same(_) => return off,
            Inner::Added(kept, _) | Inner::Removed(kept, _) => kept,
        };
        // The kept characters before and after `off`.
        let i = kept.partition_point(|k| k.0 < off);
        match side {
            Side::Start if i == 0 => new_from,
            Side::Start => kept.get(i - 1).map_or(new_from, |k| k.1 + k.2),
            Side::End => kept.get(i).map_or(new_to, |k| k.1),
        }
    }
}

/// A position's place in its story's top level; inside a table, the table's start (`Side::Start`)
/// or end.
fn key(p: &Pos, side: Side) -> Key {
    let i = p.path.0.first().copied().unwrap_or(0) as usize;
    match (p.path.0.len() > 1, side) {
        (false, _) => (i, p.off),
        (true, Side::Start) => (i, 0),
        (true, Side::End) => (i, usize::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wordcraft_doc::Document;

    fn session(text: &str) -> Session {
        let mut s = Session::new(Document::new());
        s.run("document.setText", &json!({"text": text})).unwrap();
        s
    }

    fn text(s: &Session) -> String {
        s.doc.plain_text(StoryRef::Body)
    }

    /// The text a lock covers.
    fn held(s: &Session, id: u64) -> String {
        let l = s.locks.get(id).unwrap();
        s.doc.copy_range(&l.start, &l.end).plain_text()
    }

    fn find(s: &Session, word: &str) -> (Pos, Pos) {
        let mut s2 = Session::new(s.doc.clone());
        s2.run("select.text", &json!({"text": word})).unwrap();
        s2.sel.ordered()
    }

    fn as_editor(s: &mut Session, who: Option<&str>, id: &str, params: serde_json::Value) -> Result<serde_json::Value, CmdError> {
        let before = std::mem::replace(&mut s.editor, who.map(str::to_string));
        let r = s.run(id, &params);
        s.editor = before;
        r
    }

    #[test]
    fn a_table_lock_refuses_another_agents_edit_in_any_cell() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.run("insert.table", &json!({"rows": 1, "cols": 2})).unwrap();
        let paths = s.doc.para_paths(StoryRef::Body);
        let first = paths.iter().find(|path| path.cell().is_some()).unwrap().clone();
        let second = paths.iter().filter(|path| path.cell().is_some()).nth(1).unwrap().clone();
        s.sel = crate::Selection::caret(Pos { story: StoryRef::Body, path: first, off: 0 });
        s.run("text.insert", &json!({"text": "first"})).unwrap();
        let (start, end) = find(&s, "first");
        s.lock_range(Some("Agent (alpha)"), &start, &end);
        let before = s.doc.clone();
        s.sel = crate::Selection::caret(Pos { story: StoryRef::Body, path: second, off: 0 });
        s.editor = Some("Agent (beta)".into());
        assert!(s.run("text.insert", &json!({"text": "second"})).unwrap_err().to_string().contains("Agent (alpha)"));
        assert_eq!(s.doc, before);
    }

    #[test]
    fn removing_a_held_story_is_refused_for_another_agent() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        let part = s.doc.add_part(
            wordcraft_doc::PartKind::Header,
            vec![wordcraft_doc::para_block(wordcraft_doc::Paragraph::with_text("Header", Default::default()))],
        );
        let start = Pos { story: StoryRef::Part(part), path: wordcraft_doc::Path::top(0), off: 0 };
        let end = Pos { off: 6, ..start.clone() };
        s.lock_range(Some("Agent (alpha)"), &start, &end);
        s.registry =
            std::sync::Arc::new(crate::Registry::new(vec![crate::CommandSpec::new("test.removePart", "Remove part", "Test", |s, params| {
                let part = params["part"].as_u64().unwrap() as u32;
                s.doc.parts.remove(&part);
                Ok(json!({}))
            })]));
        let before = s.doc.clone();
        s.editor = Some("Agent (beta)".into());
        assert!(s.run("test.removePart", &json!({"part": part})).unwrap_err().to_string().contains("Agent (alpha)"));
        assert_eq!(s.doc, before);
        s.editor = None;
        s.run("test.removePart", &json!({"part": part})).unwrap();
        assert!(s.locks.is_empty(), "a removed story never redirects its request to the body");
    }

    #[test]
    fn restoring_a_snapshot_restores_followed_ranges() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.run("document.setText", &serde_json::json!({"text": "one two three"})).unwrap();
        s.run("select.text", &serde_json::json!({"text": "two"})).unwrap();
        s.lock_range(Some("Agent (alpha)"), &s.sel.anchor.clone(), &s.sel.focus.clone());
        let expected = s.locks.list().to_vec();
        let snap = s.edit_snapshot();
        s.run("caret.docStart", &serde_json::json!({})).unwrap();
        s.run("text.insert", &serde_json::json!({"text": "PREFIX "})).unwrap();
        s.restore(snap);
        assert_eq!(s.locks.list(), expected);
    }

    #[test]
    fn another_agent_cannot_undo_a_held_edit() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.run("document.setText", &serde_json::json!({"text": "one two three"})).unwrap();
        s.run("select.text", &serde_json::json!({"text": "two"})).unwrap();
        s.lock_range(Some("Agent (alpha)"), &s.sel.anchor.clone(), &s.sel.focus.clone());
        s.editor = Some("Agent (alpha)".into());
        s.run("text.insert", &serde_json::json!({"text": "second"})).unwrap();
        let before = s.doc.clone();
        let locks = s.locks.list().to_vec();
        s.editor = Some("Agent (beta)".into());
        assert!(s.run("edit.undo", &serde_json::json!({})).unwrap_err().to_string().contains("Agent (alpha)"));
        assert_eq!(s.doc, before);
        assert_eq!(s.locks.list(), locks);
        s.editor = None;
        s.run("edit.undo", &serde_json::json!({})).unwrap();
        assert_eq!(s.doc.plain_text(StoryRef::Body), "one two three");
    }

    #[test]
    fn another_agents_edit_inside_a_held_range_is_refused_and_undone() {
        let mut s = session("Kilns fire clay slowly.\nGlazes melt at high heat.");
        let (a, b) = find(&s, "clay slowly");
        let lock = s.lock_range(Some("Agent (alpha)"), &a, &b);
        // Beta replaces a word inside alpha's range: refused, nothing changed, and it says who.
        let (x, y) = find(&s, "slowly");
        s.sel = crate::Selection { anchor: x, focus: y };
        let e = as_editor(&mut s, Some("Agent (beta)"), "text.insert", json!({"text": "fast"})).unwrap_err().to_string();
        assert!(e.contains("Agent (alpha) is working on") && e.contains("text.insert refused"), "{e}");
        assert_eq!(text(&s), "Kilns fire clay slowly.\nGlazes melt at high heat.");
        assert!(!e.contains("Ask Agent request"), "the bridge must not take this for a request that is over: {e}");
        // Beta edits elsewhere: fine, and alpha's range follows.
        let (x, y) = find(&s, "Kilns");
        s.sel = crate::Selection { anchor: x, focus: y };
        as_editor(&mut s, Some("Agent (beta)"), "text.insert", json!({"text": "Big kilns"})).unwrap();
        assert_eq!(held(&s, lock), "clay slowly");
        // Typing right at the range's edge isn't inside it.
        let end = s.locks.get(lock).unwrap().end.clone();
        s.sel = crate::Selection::caret(end);
        as_editor(&mut s, Some("Agent (beta)"), "text.insert", json!({"text": ","})).unwrap();
        assert_eq!(held(&s, lock), "clay slowly", "beta's comma stays outside alpha's range");
        // Alpha itself edits inside: fine, and the range covers its new text.
        let (x, y) = find(&s, "slowly");
        s.sel = crate::Selection { anchor: x, focus: y };
        as_editor(&mut s, Some("Agent (alpha)"), "text.insert", json!({"text": "gently over many hours"})).unwrap();
        assert_eq!(held(&s, lock), "clay gently over many hours");
        // The person is never refused.
        let (x, y) = find(&s, "gently");
        s.sel = crate::Selection { anchor: x, focus: y };
        s.run("text.insert", &json!({"text": "softly"})).unwrap();
        assert!(text(&s).contains("clay softly over many hours"));
        // Released (no owner): beta may edit it.
        s.locks.set_owner(lock, None);
        let (x, y) = find(&s, "softly");
        s.sel = crate::Selection { anchor: x, focus: y };
        as_editor(&mut s, Some("Agent (beta)"), "text.insert", json!({"text": "evenly"})).unwrap();
        assert!(text(&s).contains("clay evenly over"));
    }

    #[test]
    fn ranges_follow_paragraphs_added_and_removed_before_them() {
        let mut s = session("One.\nTwo.\nThree has the word.\nFour.");
        let (a, b) = find(&s, "the word");
        let lock = s.lock_range(Some("Agent (alpha)"), &a, &b);
        // New paragraphs above (the person presses Enter twice in the first one).
        s.run("caret.docStart", &json!({})).unwrap();
        s.run("text.newParagraph", &json!({})).unwrap();
        s.run("text.newParagraph", &json!({})).unwrap();
        assert_eq!(held(&s, lock), "the word");
        // A paragraph above goes (select it whole and delete).
        let (x, _) = find(&s, "Two.");
        let y = Pos { off: 0, path: wordcraft_doc::Path::top(x.path.last() + 1), ..x.clone() };
        s.sel = crate::Selection { anchor: x, focus: y };
        s.run("text.delete", &json!({})).unwrap();
        assert!(!text(&s).contains("Two."));
        assert_eq!(held(&s, lock), "the word");
        // Joining its paragraph with the one before (Backspace at its start) keeps it too.
        let (x, _) = find(&s, "Three");
        s.sel = crate::Selection::caret(x);
        s.run("text.backspace", &json!({})).unwrap();
        assert_eq!(held(&s, lock), "the word");
        // Splitting inside it keeps both halves.
        let (_, y) = find(&s, "the");
        s.sel = crate::Selection::caret(y);
        s.run("text.newParagraph", &json!({})).unwrap();
        assert_eq!(held(&s, lock), "the\n word");
        // Undo and Redo move it back and forth.
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!(held(&s, lock), "the word");
    }

    #[test]
    fn the_holders_own_text_at_its_edges_joins_the_range() {
        let mut s = session("Kilns fire clay.");
        let (a, b) = find(&s, "clay");
        let lock = s.lock_range(Some("Agent (alpha)"), &a, &b);
        // Alpha replaces the whole range, then adds a paragraph after it (insertBelow).
        s.sel = crate::Selection { anchor: a, focus: b };
        as_editor(&mut s, Some("Agent (alpha)"), "text.insert", json!({"text": "stoneware"})).unwrap();
        assert_eq!(held(&s, lock), "stoneware");
        let end = s.locks.get(lock).unwrap().end.clone();
        s.sel = crate::Selection::caret(end);
        as_editor(&mut s, Some("Agent (alpha)"), "text.insert", json!({"text": " and porcelain"})).unwrap();
        assert_eq!(held(&s, lock), "stoneware and porcelain");
        // Formatting by another agent inside the range is an edit too.
        let (x, y) = find(&s, "porcelain");
        s.sel = crate::Selection { anchor: x, focus: y };
        assert!(as_editor(&mut s, Some("Agent (beta)"), "format.bold", json!({})).is_err());
        assert!(as_editor(&mut s, Some("Agent (beta)"), "para.alignCenter", json!({})).is_err(), "so is its paragraph's formatting");
        // A new document lets every range go.
        s.run("file.new", &json!({})).unwrap();
        assert!(s.locks.is_empty());
    }

    /// Marks put in at two places at once (a comment around a range) move a range that starts
    /// between them by exactly what went in before it; reformatting moves nothing.
    #[test]
    fn edits_at_two_places_in_a_paragraph_move_ranges_exactly() {
        let mut s = session("Kilns fire the clay slowly over many hours.");
        let (a, b) = find(&s, "the clay slowly");
        let first = s.lock_range(Some("Agent (alpha)"), &a, &b);
        let (a, b) = find(&s, "slowly over many hours");
        let second = s.lock_range(Some("Agent (beta)"), &a, &b);
        let (a, b) = find(&s, "the clay slowly");
        s.sel = crate::Selection { anchor: a, focus: b };
        s.run("review.newComment", &json!({"text": "note"})).unwrap();
        let obj = wordcraft_doc::para::OBJ.to_string();
        assert_eq!(held(&s, second).replace(&obj, ""), "slowly over many hours");
        assert!(held(&s, second).starts_with("slowly"), "{:?}", held(&s, second));
        assert_eq!(held(&s, first).replace(&obj, ""), "the clay slowly");
        // Bold across both ranges by the person: offsets stay.
        let (a, b) = find(&s, "clay slowly");
        s.sel = crate::Selection { anchor: a, focus: b };
        s.run("format.bold", &json!({})).unwrap();
        assert!(held(&s, second).starts_with("slowly"));
        // Beta may format its own words, not alpha's.
        let (a, b) = find(&s, "hours");
        s.sel = crate::Selection { anchor: a, focus: b };
        assert!(as_editor(&mut s, Some("Agent (beta)"), "format.italic", json!({})).is_ok());
        let (a, b) = find(&s, "clay");
        s.sel = crate::Selection { anchor: a, focus: b };
        assert!(as_editor(&mut s, Some("Agent (beta)"), "format.italic", json!({})).is_err());
    }

    #[test]
    fn junk_and_tables_never_panic() {
        let mut s = session("A\nB");
        let far = Pos::body(99, 1234);
        let id = s.lock_range(Some("Agent (alpha)"), &far, &Pos::body(0, 0));
        assert!(s.locks.get(id).is_some());
        s.run("caret.docEnd", &json!({})).unwrap();
        s.run("insert.table", &json!({"rows": 2, "cols": 2})).unwrap();
        s.run("text.insert", &json!({"text": "cell"})).unwrap();
        let (a, b) = find(&s, "cell");
        let t = s.lock_range(Some("Agent (alpha)"), &a, &b);
        assert!(a.path.0.len() > 1, "in the table: {a:?}");
        s.run("caret.docStart", &json!({})).unwrap();
        s.run("text.newParagraph", &json!({})).unwrap();
        assert_eq!(held(&s, t), "cell", "a range in a table follows the table");
        for _ in 0..3 {
            s.run("edit.undo", &json!({})).unwrap();
        }
        for l in s.locks.list() {
            assert!(s.doc.para_at(&l.start).is_some() && s.doc.para_at(&l.end).is_some(), "{l:?}");
        }
        assert!(s.locks.remove(t).is_some() && s.locks.remove(t).is_none());
        for i in 0..(MAX_LOCKS + 5) {
            s.lock_range(None, &Pos::body(0, i), &Pos::body(0, i));
        }
        assert!(s.locks.list().len() <= MAX_LOCKS);
    }
}
