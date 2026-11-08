//! `telepager install-hooks`: write the "page me when you're done" guidance
//! into the files coding agents read, and print how to register the MCP server.
//!
//! Re-running it is safe: the block is marker-delimited, so it is replaced in
//! place instead of being appended again.

use std::path::{Path, PathBuf};

const START: &str = "<!-- telepager -->";
const END: &str = "<!-- /telepager -->";

/// The command that registers the stdio server with Claude Code.
pub const REGISTRATION_COMMAND: &str = "claude mcp add --scope user telepager -- telepager mcp";

/// Which agent files a run should touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Opencode,
    Cursor,
    All,
}

impl Agent {
    /// Parse the `--agent` value. `None` for anything unrecognised.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "claude" => Some(Self::Claude),
            "opencode" => Some(Self::Opencode),
            "cursor" => Some(Self::Cursor),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// The prompt block installed into a guidance file.
pub fn hooks_snippet() -> &'static str {
    "<!-- telepager -->\n\
When you finish a long task, use telepager's `send_message` to page the user. When blocked on a decision use `ask_question`. Use `send_thinking` for progress.\n\
If telepager's MCP server is available, always page on completion, including when the user follows up interactively.\n\
<!-- /telepager -->\n"
}

/// The guidance file one agent reads. `All` reports the primary file; use
/// [`targets`] when you need every file a run will write.
pub fn target_file(agent: Agent, dir: &Path) -> PathBuf {
    match agent {
        Agent::Opencode => dir.join("AGENTS.md"),
        Agent::Claude | Agent::Cursor | Agent::All => dir.join("CLAUDE.md"),
    }
}

/// Every guidance file a run for `agent` will write.
pub fn targets(agent: Agent, dir: &Path) -> Vec<PathBuf> {
    match agent {
        Agent::All => vec![target_file(Agent::Claude, dir), target_file(Agent::Opencode, dir)],
        one => vec![target_file(one, dir)],
    }
}

/// Pick an agent from the files already in `dir`. Defaults to Claude, which
/// means "write CLAUDE.md".
pub fn detect(dir: &Path) -> Agent {
    let has_claude = dir.join("CLAUDE.md").exists();
    if !has_claude && dir.join("AGENTS.md").exists() {
        return Agent::Opencode;
    }
    if !has_claude && dir.join(".cursorrules").exists() {
        return Agent::Cursor;
    }
    Agent::Claude
}

/// Add `snippet` to `existing`, replacing an earlier telepager block if there
/// is one. Pure so the idempotence is easy to test.
pub fn append_idempotent(existing: &str, snippet: &str) -> String {
    let snippet = snippet.trim_matches('\n');

    if let Some((start, end)) = block_range(existing) {
        let mut out = String::new();
        let before = existing[..start].trim_end_matches('\n');
        if !before.is_empty() {
            out.push_str(before);
            out.push_str("\n\n");
        }
        out.push_str(snippet);
        let after = existing[end..].trim_matches('\n');
        if !after.is_empty() {
            out.push_str("\n\n");
            out.push_str(after);
        }
        out.push('\n');
        return out;
    }

    let mut out = existing.trim_end_matches('\n').to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(snippet);
    out.push('\n');
    out
}

fn block_range(text: &str) -> Option<(usize, usize)> {
    let start = text.find(START)?;
    let end = text[start..].find(END)? + start + END.len();
    Some((start, end))
}

/// Write the guidance into `dir`. `agent` of `None` auto-detects from the files
/// already there. Returns the files touched, in write order.
pub fn install(dir: &Path, agent: Option<Agent>) -> std::io::Result<Vec<PathBuf>> {
    let agent = agent.unwrap_or_else(|| detect(dir));
    let mut written = Vec::new();

    for path in targets(agent, dir) {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = append_idempotent(&existing, hooks_snippet());
        if updated != existing {
            std::fs::write(&path, updated)?;
        }
        written.push(path);
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_snippet_tells_the_agent_to_use_every_tool() {
        let snippet = hooks_snippet();
        for tool in ["send_message", "ask_question", "send_thinking"] {
            assert!(snippet.contains(tool), "snippet doesn't mention {tool}");
        }
    }

    #[test]
    fn the_snippet_is_marker_delimited() {
        let snippet = hooks_snippet();
        assert!(snippet.contains("<!-- telepager -->"));
        assert!(snippet.contains("<!-- /telepager -->"));
        assert!(snippet.contains("always page on completion"));
    }

    #[test]
    fn appending_twice_is_the_same_as_once() {
        let once = append_idempotent("", hooks_snippet());
        let twice = append_idempotent(&once, hooks_snippet());
        assert_eq!(once, twice);
    }

    #[test]
    fn existing_content_survives_an_append() {
        let out = append_idempotent("# my project\n\nsome rules\n", hooks_snippet());
        assert!(out.starts_with("# my project\n\nsome rules"));
        assert!(out.contains(hooks_snippet()));
    }

    #[test]
    fn an_old_block_is_replaced_rather_than_duplicated() {
        let old = "<!-- telepager -->\nstale text\n<!-- /telepager -->\n";
        let out = append_idempotent(old, hooks_snippet());
        assert!(!out.contains("stale text"));
        assert_eq!(out.matches("<!-- telepager -->").count(), 1);
        assert_eq!(out.matches("<!-- /telepager -->").count(), 1);
    }

    #[test]
    fn each_agent_gets_the_file_it_reads() {
        let dir = Path::new("/tmp/project");
        assert_eq!(target_file(Agent::Claude, dir), PathBuf::from("/tmp/project/CLAUDE.md"));
        assert_eq!(target_file(Agent::Cursor, dir), PathBuf::from("/tmp/project/CLAUDE.md"));
        assert_eq!(target_file(Agent::Opencode, dir), PathBuf::from("/tmp/project/AGENTS.md"));
    }

    #[test]
    fn all_writes_both_guidance_files() {
        let files = targets(Agent::All, Path::new("/tmp/project"));
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|p| p.ends_with("CLAUDE.md")));
        assert!(files.iter().any(|p| p.ends_with("AGENTS.md")));
    }

    #[test]
    fn agent_names_parse_and_anything_else_is_none() {
        assert_eq!(Agent::parse("claude"), Some(Agent::Claude));
        assert_eq!(Agent::parse("opencode"), Some(Agent::Opencode));
        assert_eq!(Agent::parse("cursor"), Some(Agent::Cursor));
        assert_eq!(Agent::parse("all"), Some(Agent::All));
        assert_eq!(Agent::parse("emacs"), None);
    }

    #[test]
    fn install_is_idempotent_on_disk() {
        let dir = std::env::temp_dir().join(format!("telepager-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = install(&dir, Some(Agent::All)).unwrap();
        assert_eq!(first.len(), 2);
        let claude = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        let agents = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();

        install(&dir, Some(Agent::All)).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap(), claude);
        assert_eq!(std::fs::read_to_string(dir.join("AGENTS.md")).unwrap(), agents);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
