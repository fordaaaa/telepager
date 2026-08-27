//! Coding agents running in tmux panes telepager didn't start. Discovery is a
//! `tmux list-panes` away, and every pane whose foreground command is one of
//! our agent presets becomes a session the master can read and type at.
//!
//! Off unless `master.tmux_attach` is on, and best-effort throughout: no tmux
//! binary, no server, or a tmux that errors just means an empty list. Nothing
//! here ever fails session enumeration.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tokio::process::Command;

use crate::config::AgentPreset;

const PANE_FORMAT: &str =
    "#{pane_id}\t#{pane_current_command}\t#{pane_current_path}\t#{session_name}:#{window_index}.#{pane_index}";

/// How much scrollback `read_output` pulls back from a pane.
pub const CAPTURE_LINES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// tmux's own id, e.g. `%12`. Stable for the life of the pane.
    pub id: String,
    /// The foreground command, which is also the agent name we matched on.
    pub command: String,
    pub cwd: Option<PathBuf>,
    /// `session:window.pane`, for display.
    pub location: String,
}

/// The binary names that count as a coding agent, taken from the presets so
/// there's one list rather than two.
pub fn agent_commands(presets: &std::collections::BTreeMap<String, AgentPreset>) -> BTreeSet<String> {
    presets
        .values()
        .filter_map(|p| {
            let name = PathBuf::from(p.command.trim())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())?;
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// Every tmux pane whose foreground command is a known agent. Empty on
/// windows, without tmux, or with no server running.
pub async fn discover(presets: &std::collections::BTreeMap<String, AgentPreset>) -> Vec<Pane> {
    if cfg!(windows) {
        return Vec::new();
    }
    let wanted = agent_commands(presets);
    match run(&["list-panes", "-a", "-F", PANE_FORMAT]).await {
        Some(text) => parse(&text, &wanted),
        None => Vec::new(),
    }
}

pub fn parse(text: &str, wanted: &BTreeSet<String>) -> Vec<Pane> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let id = parts.next()?.trim();
            let command = parts.next()?.trim();
            let cwd = parts.next().unwrap_or("").trim();
            let location = parts.next().unwrap_or("").trim();
            if !id.starts_with('%') || !wanted.contains(command) {
                return None;
            }
            Some(Pane {
                id: id.to_string(),
                command: command.to_string(),
                cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
                location: if location.is_empty() { id.to_string() } else { location.to_string() },
            })
        })
        .collect()
}

/// The tail of a pane's screen.
pub async fn capture(pane_id: &str, lines: usize) -> Option<String> {
    let start = format!("-{lines}");
    run(&["capture-pane", "-p", "-t", pane_id, "-S", &start]).await
}

/// Type `text` at a pane and press enter. `-l` keeps tmux from reading the
/// text as key names, and the argument vector keeps it away from a shell.
pub async fn send_text(pane_id: &str, text: &str) -> Result<(), String> {
    run_checked(&["send-keys", "-t", pane_id, "-l", "--", text]).await?;
    run_checked(&["send-keys", "-t", pane_id, "Enter"]).await
}

/// Ctrl-C, and only Ctrl-C. This is the user's own terminal, not something
/// telepager spawned, so interrupting the agent is the ceiling — we never
/// close the pane or kill the process behind it.
pub async fn interrupt(pane_id: &str) -> Result<(), String> {
    run_checked(&["send-keys", "-t", pane_id, "C-c"]).await
}

/// Bring the registry in line with what tmux is running: open a session for
/// every new agent pane, forget the ones whose pane has gone. A no-op unless
/// `master.tmux_attach` is on, so nothing shells out to tmux by default.
pub async fn sync(core: &std::sync::Arc<crate::core::Core>) {
    let Some(cfg) = core.config().await else {
        return;
    };
    if !cfg.master.tmux_attach {
        // turned off mid-run: drop what an earlier sync found rather than
        // leave sessions the master can still act on
        for summary in core.sessions().await {
            if summary["pane"].is_string() {
                if let Some(id) = summary["id"].as_str() {
                    core.forget_session(id).await;
                }
            }
        }
        return;
    }

    let panes = discover(&cfg.agents).await;
    let mut tracked = BTreeSet::new();

    for summary in core.sessions().await {
        let Some(pane) = summary["pane"].as_str() else {
            continue;
        };
        if panes.iter().any(|p| p.id == pane) {
            tracked.insert(pane.to_string());
        } else if let Some(id) = summary["id"].as_str() {
            core.forget_session(id).await;
        }
    }

    for pane in panes {
        if tracked.contains(&pane.id) {
            continue;
        }
        let label = pane
            .cwd
            .as_ref()
            .and_then(|c| c.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| pane.location.clone());
        core.open_session(
            crate::session::Kind::TmuxAttached {
                pane_id: pane.id.clone(),
                location: pane.location.clone(),
            },
            label,
            pane.cwd.clone(),
            Some(pane.command.clone()),
            None,
        )
        .await;
    }
}

/// The pane behind a session id, if that session is a tmux one. Tool handlers
/// look a pane up this way rather than trusting anything in their arguments.
pub async fn pane_of(core: &std::sync::Arc<crate::core::Core>, session: &str) -> Option<String> {
    if !core.config().await.is_some_and(|c| c.master.tmux_attach) {
        return None;
    }
    core.sessions()
        .await
        .into_iter()
        .find(|s| s["id"].as_str() == Some(session))
        .and_then(|s| s["pane"].as_str().map(str::to_string))
}

async fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn run_checked(args: &[&str]) -> Result<(), String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("tmux could not be run: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() { "tmux failed".into() } else { err })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted() -> BTreeSet<String> {
        ["claude", "codex"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_panes_running_a_known_agent_are_candidates() {
        let text = "%1\tclaude\t/home/me/proj\twork:0.0\n\
                    %2\tzsh\t/home/me\twork:0.1\n\
                    %3\tcodex\t/srv/api\tother:2.3\n";
        let found = parse(text, &wanted());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "%1");
        assert_eq!(found[0].command, "claude");
        assert_eq!(found[0].cwd, Some(PathBuf::from("/home/me/proj")));
        assert_eq!(found[1].location, "other:2.3");
    }

    #[test]
    fn junk_and_empty_output_parse_to_nothing() {
        assert!(parse("", &wanted()).is_empty());
        assert!(parse("no server running on /tmp/tmux-1000/default", &wanted()).is_empty());
        // an id that isn't a pane id is not one of ours
        assert!(parse("1\tclaude\t/x\ta:0.0", &wanted()).is_empty());
    }

    #[test]
    fn a_pane_with_no_path_or_location_still_parses() {
        let found = parse("%9\tclaude\t\t", &wanted());
        assert_eq!(found.len(), 1);
        assert!(found[0].cwd.is_none());
        assert_eq!(found[0].location, "%9");
    }

    #[test]
    fn the_agent_names_come_from_the_presets() {
        let names = agent_commands(&crate::config::builtin_agents());
        assert!(names.contains("claude"));
        assert!(names.contains("opencode"));
        // presets sharing a binary collapse to one name
        assert_eq!(names.iter().filter(|n| *n == "claude").count(), 1);
    }

    #[tokio::test]
    async fn discovery_never_errors_when_tmux_is_missing() {
        // whatever this machine has, discovery returns a list and not an error
        let _ = discover(&crate::config::builtin_agents()).await;
    }
}
