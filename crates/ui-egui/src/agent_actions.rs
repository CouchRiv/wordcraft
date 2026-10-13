//! Ask Agent's quick actions: one click on a selection sends a ready-made request. Each action is
//! data, not code: `{id, label, menu, instruction, apply}`. The request carries the action's id,
//! its instruction in plain English (so any agent knows what to do without knowing WordCraft's
//! ids) and what to do with the result (`apply`). Labels are English and shown through the
//! interface's catalogs; the list is serializable so it can later be edited in File › Options.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What the agent does with its result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Apply {
    /// The agent decides from the instruction (a typed request).
    #[default]
    Auto,
    /// Replace the selection with the result.
    Replace,
    /// Insert the result after the selection, as new paragraphs; the selection stays.
    InsertBelow,
    /// Change nothing: the answer goes in the reply only. The window refuses the agent's edits.
    ReplyOnly,
}

impl Apply {
    pub fn name(self) -> &'static str {
        match self {
            Apply::Auto => "auto",
            Apply::Replace => "replace",
            Apply::InsertBelow => "insertBelow",
            Apply::ReplyOnly => "replyOnly",
        }
    }
}

/// One quick action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAction {
    /// Stable id the request carries (`shorten`, `rewrite.professional`, `translate.fr`).
    pub id: String,
    /// English label, shown translated.
    pub label: String,
    /// The submenu it sits in (`Rewrite`, `Translate`, `More`), or `None` for the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<String>,
    /// What the agent is asked, in plain English.
    pub instruction: String,
    pub apply: Apply,
}

impl AgentAction {
    /// `{id, label}` as a request carries it.
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "label": self.label})
    }
}

/// The submenus, in order, after the top-level actions that come before them.
pub const MENUS: [&str; 3] = ["Rewrite", "Translate", "More"];

/// Tones for Rewrite: (id, label, how).
const TONES: [(&str, &str, &str); 4] = [
    ("professional", "Professional", "a professional, polished tone"),
    ("concise", "Concise", "a concise tone: fewer words, nothing important lost"),
    ("friendly", "Friendly", "a warm, friendly tone"),
    ("confident", "Confident", "a confident, assertive tone, without hedging"),
];

/// Languages for Translate: (BCP 47 tag, label, English name used in the instruction).
const LANGUAGES: [(&str, &str, &str); 8] = [
    ("en", "English", "English"),
    ("fr", "French", "French"),
    ("es", "Spanish", "Spanish"),
    ("de", "German", "German"),
    ("it", "Italian", "Italian"),
    ("pt", "Portuguese", "Portuguese"),
    ("zh-Hans", "Chinese (Simplified)", "Simplified Chinese"),
    ("ja", "Japanese", "Japanese"),
];

/// The built-in actions, in menu order.
pub fn defaults() -> Vec<AgentAction> {
    let a = |id: String, label: &str, menu: Option<&str>, instruction: String, apply: Apply| AgentAction {
        id,
        label: label.into(),
        menu: menu.map(Into::into),
        instruction,
        apply,
    };
    let keep = "Keep its meaning, its facts and its language.";
    let mut v: Vec<AgentAction> = TONES
        .iter()
        .map(|(id, label, how)| {
            a(format!("rewrite.{id}"), label, Some("Rewrite"), format!("Rewrite the selected text in {how}. {keep}"), Apply::Replace)
        })
        .collect();
    v.extend([
        a("shorten".into(), "Shorten", None, format!("Shorten the selected text to about half its length. {keep}"), Apply::Replace),
        a(
            "expand".into(),
            "Expand",
            None,
            "Expand the selected text with more detail and explanation, in the same tone and language. Don't invent facts.".into(),
            Apply::Replace,
        ),
        a(
            "fixGrammar".into(),
            "Fix spelling & grammar",
            None,
            "Fix the spelling, grammar and punctuation of the selected text. Change nothing else.".into(),
            Apply::Replace,
        ),
        a(
            "simplify".into(),
            "Simplify",
            None,
            format!("Rewrite the selected text in plain words and short sentences, for a general reader. {keep}"),
            Apply::Replace,
        ),
    ]);
    v.extend(LANGUAGES.iter().map(|(tag, label, name)| {
        a(
            format!("translate.{tag}"),
            label,
            Some("Translate"),
            format!("Translate the selected text into {name} ({tag}). Keep its meaning, tone and formatting."),
            Apply::Replace,
        )
    }));
    v.extend([
        a(
            "bulletize".into(),
            "Bulletize",
            Some("More"),
            "Turn the selected text into a bulleted list of its main points, one short item each, in its language.".into(),
            Apply::Replace,
        ),
        a(
            "table".into(),
            "Turn into table",
            Some("More"),
            "Turn the selected text into a table: choose sensible columns, one row per item, with a header row.".into(),
            Apply::Replace,
        ),
        a(
            "summarize".into(),
            "Summarize",
            Some("More"),
            "Summarize the selected text in a few sentences, in its language. Insert the summary as a new paragraph after the selection, and leave the selection as it is.".into(),
            Apply::InsertBelow,
        ),
        a(
            "continue".into(),
            "Continue writing",
            Some("More"),
            "Continue writing after the selected text: one or two paragraphs in the same voice, tone and language, inserted after the selection.".into(),
            Apply::InsertBelow,
        ),
        a(
            "explain".into(),
            "Explain",
            Some("More"),
            "Explain the selected text in plain words: what it means and why it matters. Don't change the document; answer in your reply only.".into(),
            Apply::ReplyOnly,
        ),
    ]);
    v
}

/// The action with this id.
pub fn find(id: &str) -> Option<AgentAction> {
    defaults().into_iter().find(|a| a.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_actions_are_complete_and_unique() {
        let all = defaults();
        let mut ids: Vec<&str> = all.iter().map(|a| a.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len(), "ids are unique");
        for id in [
            "rewrite.professional",
            "rewrite.concise",
            "rewrite.friendly",
            "rewrite.confident",
            "shorten",
            "expand",
            "fixGrammar",
            "simplify",
            "translate.fr",
            "bulletize",
            "table",
            "summarize",
            "continue",
            "explain",
        ] {
            assert!(find(id).is_some(), "{id}");
        }
        for a in &all {
            assert!(a.menu.as_deref().is_none_or(|m| MENUS.contains(&m)), "{}: menu {:?}", a.id, a.menu);
            assert!(a.instruction.len() > 20 && a.instruction.ends_with('.'), "{}: a plain sentence", a.id);
            assert!(a.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'), "{}", a.id);
        }
        assert_eq!(find("explain").unwrap().apply, Apply::ReplyOnly);
        assert_eq!(find("summarize").unwrap().apply, Apply::InsertBelow);
        assert!(find("translate.fr").unwrap().instruction.contains("French"));
        assert!(find("nope").is_none());
    }

    #[test]
    fn actions_round_trip_as_data() {
        let all = defaults();
        let v = serde_json::to_value(&all).unwrap();
        assert_eq!(v[0]["apply"], "replace");
        assert_eq!(serde_json::from_value::<Vec<AgentAction>>(v).unwrap(), all);
        let typed: AgentAction = serde_json::from_value(json!({"id": "x", "label": "X", "instruction": "Do x.", "apply": "insertBelow"})).unwrap();
        assert_eq!((typed.menu, typed.apply), (None, Apply::InsertBelow));
    }
}
