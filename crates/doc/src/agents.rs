//! Agents as authors. An agent that edits for the person (Ask Agent) signs its tracked changes and
//! comments `Agent (<name>)`, where `<name>` is how the agent names itself (an MCP client's
//! `clientInfo.name`, or a name given explicitly). The author is the only thing a revision keeps
//! in a file (`w:author`), so this form is what tells an agent's marks from a person's in any
//! document, saved and opened again, and what gives each agent the same colour everywhere.

/// The author an agent called `name` signs with: `Agent (<name>)`; plain `Agent` without a name.
pub fn author(name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("Agent ({name})"),
        None => "Agent".into(),
    }
}

/// The agent's name in an author written by [`author`]: `Some("")` for a bare `Agent`, `None`
/// for anyone else (a person, or another program that signs differently).
pub fn name_of(author: &str) -> Option<&str> {
    let a = author.trim();
    if a == "Agent" {
        return Some("");
    }
    a.strip_prefix("Agent (").and_then(|rest| rest.strip_suffix(')')).map(str::trim)
}

/// Whether `author` is an agent's ([`name_of`]).
pub fn is_agent(author: &str) -> bool {
    name_of(author).is_some()
}

/// A number that only depends on the agent's name (FNV-1a over its lower-cased characters): the
/// same agent gets the same colour in every document, every session and on every surface.
pub fn name_hash(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        let mut buf = [0u8; 4];
        for b in c.encode_utf8(&mut buf).bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authors_round_trip_and_people_are_not_agents() {
        assert_eq!(author(Some("  claude-code ")), "Agent (claude-code)");
        assert_eq!(author(Some("   ")), "Agent");
        assert_eq!(author(None), "Agent");
        assert_eq!(name_of("Agent (claude-code)"), Some("claude-code"));
        assert_eq!(name_of(" Agent "), Some(""));
        assert_eq!(name_of("Agent (a (b))"), Some("a (b)"));
        for person in ["Ana Writer", "Agentha", "Agent Smith", "Agent (unclosed", ""] {
            assert!(!is_agent(person), "{person:?}");
        }
    }

    #[test]
    fn the_hash_follows_the_name_only() {
        assert_eq!(name_hash("Claude Code"), name_hash(" claude code "));
        assert_ne!(name_hash("alpha"), name_hash("beta"));
        // Stable across releases: documents and screenshots keep their colours.
        assert_eq!(name_hash(""), 0x811c_9dc5);
        assert_eq!(name_hash("a"), 0xe40c_292c);
    }
}
