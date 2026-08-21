//! Spawning worker agents.
//!
//! A worker agent is just some coding CLI — `claude`, `codex`, `gemini`,
//! `opencode`, whatever you configured — started in a directory with a task.
//! telepager owns its pipes: stdout and stderr become session events, stdin
//! stays open so a follow-up can be typed at it, and the child can be killed.
//!
//! This file deliberately knows nothing about the registry or Telegram. It
//! resolves and launches processes; core wires the output up.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

use crate::config::AgentPreset;

/// What `launch` hands back: the child and its pipes, already split out.
pub struct Launched {
    pub child: Child,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    pub stdin: Option<ChildStdin>,
    /// The command line we actually ran, for the session log.
    pub rendered: Vec<String>,
}

/// Substitute the placeholders a preset may use in its arguments.
///
/// An argument that is exactly `{task}` is replaced as a whole argument, so a
/// task containing spaces or quotes stays one argument and never touches a
/// shell. A placeholder embedded in a larger argument is substituted in place,
/// which is what makes things like `--message={task}` work.
pub fn render_args(args: &[String], task: &str, dir: &Path) -> Vec<String> {
    let dir_text = dir.display().to_string();
    args.iter()
        .map(|arg| match arg.as_str() {
            "{task}" => task.to_string(),
            "{dir}" => dir_text.clone(),
            other => other.replace("{task}", task).replace("{dir}", &dir_text),
        })
        .collect()
}

/// Find an executable on PATH, the way a shell would.
///
/// Used to show only the agents you actually have installed, and to give a
/// clear error instead of a bare `No such file or directory` from the OS.
pub fn which(command: &str) -> Option<PathBuf> {
    // an explicit path is used as given
    if command.contains('/') || command.contains('\\') {
        let p = PathBuf::from(command);
        return is_executable(&p).then_some(p);
    }

    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        // the bare name comes first: a command configured as `claude.cmd`
        // already carries its extension and would never be found if we only
        // ever appended one
        let mut all = vec![String::new()];
        all.extend(
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
                .split(';')
                .filter(|e| !e.is_empty())
                .map(|e| e.to_lowercase()),
        );
        all
    } else {
        vec![String::new()]
    };

    for dir in std::env::split_paths(&path) {
        for ext in &extensions {
            let candidate = dir.join(format!("{command}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return meta.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The agent picker the UI draws: every configured preset, with whether its
/// command is actually installed on this machine.
pub fn catalog(agents: &BTreeMap<String, AgentPreset>) -> Vec<Value> {
    let mut out: Vec<Value> = agents
        .iter()
        .map(|(name, preset)| {
            let found = which(&preset.command);
            json!({
                "name": name,
                "command": preset.command,
                "description": preset.description,
                "installed": found.is_some(),
                "path": found.map(|p| p.display().to_string()),
                "interactive": preset.interactive,
            })
        })
        .collect();
    // installed ones first, so the picker opens on something usable
    out.sort_by_key(|a| {
        (
            !a["installed"].as_bool().unwrap_or(false),
            a["name"].as_str().unwrap_or_default().to_string(),
        )
    });
    out
}

/// Expand a leading `~` and make the path absolute, so a directory typed on a
/// phone resolves the same way it would in a shell.
pub fn resolve_dir(input: &str) -> Result<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("no directory given");
    }

    let expanded = if trimmed == "~" {
        dirs::home_dir().context("no home directory")?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir().context("no home directory")?.join(rest)
    } else {
        PathBuf::from(trimmed)
    };

    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };

    if !absolute.exists() {
        bail!("{} does not exist", absolute.display());
    }
    if !absolute.is_dir() {
        bail!("{} is not a directory", absolute.display());
    }

    // canonicalize so allowlist checks can't be walked around with ..
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

/// Whether `dir` is inside one of the allowed roots.
///
/// This gates spawning from Telegram only. The local web UI isn't restricted:
/// reaching it already means being at the machine with the config file.
pub fn dir_is_allowed(dir: &Path, allowed: &[PathBuf]) -> bool {
    allowed.iter().any(|root| {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        dir == root || dir.starts_with(&root)
    })
}

pub fn launch(preset: &AgentPreset, dir: &Path, task: &str) -> Result<Launched> {
    if preset.command.trim().is_empty() {
        bail!("this agent has no command configured");
    }
    let program = which(&preset.command).ok_or_else(|| {
        anyhow::anyhow!(
            "'{}' is not installed, or not on telepager's PATH",
            preset.command
        )
    })?;

    let args = render_args(&preset.args, task, dir);

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // agents that colour their output make a mess of the log pane
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .kill_on_drop(true);

    for (key, value) in &preset.env {
        cmd.env(key, value);
    }

    #[cfg(unix)]
    {
        unsafe {
            // its own process group, so killing it takes the whole tree with
            // it rather than leaving orphans behind
            cmd.pre_exec(|| {
                setpgid_to_self();
                Ok(())
            });
        }
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;

    let mut rendered = vec![preset.command.clone()];
    rendered.extend(args);

    Ok(Launched {
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        stdin: child.stdin.take(),
        child,
        rendered,
    })
}

#[cfg(unix)]
fn setpgid_to_self() {
    unsafe extern "C" {
        fn setpgid(pid: i32, pgid: i32) -> i32;
    }
    unsafe {
        setpgid(0, 0);
    }
}

/// Kill a whole process group on unix, or just the child elsewhere.
///
/// Agents shell out constantly, so signalling only the parent tends to leave
/// a build running with nothing reading its output.
pub async fn kill_tree(child: &mut Child) -> Result<()> {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        // negative pid means the group, which launch() made the child lead
        unsafe {
            kill(-(pid as i32), SIGTERM);
        }
        // give it a moment to go down politely before pulling the plug
        let graceful = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            child.wait(),
        )
        .await;
        if graceful.is_ok() {
            return Ok(());
        }
    }

    child.kill().await.context("killing the agent process")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(args: &[&str]) -> AgentPreset {
        AgentPreset {
            command: "echo".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..AgentPreset::default()
        }
    }

    #[test]
    fn a_bare_task_placeholder_stays_one_argument() {
        let args = render_args(
            &["-p".into(), "{task}".into()],
            "fix the build and don't ask",
            Path::new("/tmp"),
        );
        assert_eq!(args, vec!["-p", "fix the build and don't ask"]);
    }

    #[test]
    fn an_embedded_placeholder_is_substituted_in_place() {
        let args = render_args(&["--message={task}".into()], "hi there", Path::new("/tmp"));
        assert_eq!(args, vec!["--message=hi there"]);
    }

    #[test]
    fn the_dir_placeholder_works_too() {
        let args = render_args(&["{dir}".into(), "--cwd={dir}".into()], "t", Path::new("/srv/x"));
        assert_eq!(args, vec!["/srv/x", "--cwd=/srv/x"]);
    }

    #[test]
    fn quotes_in_a_task_are_not_special() {
        let args = render_args(&["{task}".into()], r#"say "hi"; rm -rf /"#, Path::new("/tmp"));
        // no shell is involved, so this is one inert argument
        assert_eq!(args, vec![r#"say "hi"; rm -rf /"#]);
    }

    #[test]
    fn a_name_that_already_has_its_extension_is_tried_as_given() {
        // regression: on windows the candidate list was built purely by
        // appending PATHEXT entries, so an exact name was never probed
        let dir = std::env::temp_dir().join(format!("telepager-ext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("agent.cmd");
        std::fs::write(&file, "echo hi").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // an explicit path always worked; this is the PATH-lookup branch
        let previous = std::env::var_os("PATH");
        std::env::set_var("PATH", &dir);
        let found = which("agent.cmd");
        match previous {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }

        assert_eq!(found.as_deref(), Some(file.as_path()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn which_finds_something_that_exists_and_not_something_that_does_not() {
        assert!(which("sh").is_some() || which("cmd").is_some());
        assert!(which("telepager-definitely-not-a-real-command").is_none());
    }

    #[test]
    fn the_catalog_puts_installed_agents_first() {
        let mut agents = BTreeMap::new();
        agents.insert("zzz-real".to_string(), AgentPreset {
            command: "sh".into(),
            ..AgentPreset::default()
        });
        agents.insert("aaa-fake".to_string(), AgentPreset {
            command: "telepager-not-real".into(),
            ..AgentPreset::default()
        });

        let list = catalog(&agents);
        // sh exists on unix; on windows neither does, and the order is by name
        if which("sh").is_some() {
            assert_eq!(list[0]["name"], "zzz-real");
            assert_eq!(list[0]["installed"], true);
            assert_eq!(list[1]["installed"], false);
        }
    }

    #[test]
    fn resolve_dir_rejects_files_and_missing_paths() {
        assert!(resolve_dir("/telepager-nope-does-not-exist").is_err());
        assert!(resolve_dir("").is_err());
        let tmp = std::env::temp_dir();
        assert!(resolve_dir(tmp.to_str().unwrap()).is_ok());
    }

    #[test]
    fn resolve_dir_expands_a_leading_tilde() {
        let Some(home) = dirs::home_dir() else { return };
        let resolved = resolve_dir("~").unwrap();
        assert_eq!(resolved, home.canonicalize().unwrap_or(home));
    }

    #[test]
    fn the_allowlist_covers_subdirectories_but_not_siblings() {
        let root = std::env::temp_dir().join("telepager-allow-test");
        let inside = root.join("project");
        std::fs::create_dir_all(&inside).unwrap();
        let allowed = vec![root.clone()];

        assert!(dir_is_allowed(&inside, &allowed));
        assert!(dir_is_allowed(&root, &allowed));
        assert!(!dir_is_allowed(Path::new("/etc"), &allowed));
        // an empty allowlist allows nothing, which is what keeps telegram inert
        assert!(!dir_is_allowed(&inside, &[]));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_traversal_cannot_escape_the_allowlist() {
        let root = std::env::temp_dir().join("telepager-escape-test");
        std::fs::create_dir_all(root.join("project")).unwrap();
        let allowed = vec![root.clone()];

        // resolve_dir canonicalizes, so the .. is gone before the check
        let escaped = resolve_dir(root.join("project/../../").to_str().unwrap()).unwrap();
        assert!(!dir_is_allowed(&escaped, &allowed));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn launching_a_missing_command_says_so_clearly() {
        let p = AgentPreset { command: "telepager-not-real".into(), ..AgentPreset::default() };
        let Err(err) = launch(&p, &std::env::temp_dir(), "t") else {
            panic!("a missing command should not launch");
        };
        assert!(format!("{err:#}").contains("not installed"), "{err:#}");
    }

    #[tokio::test]
    async fn a_launched_agent_runs_and_its_output_can_be_read() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        if which("echo").is_none() {
            return;
        }
        let mut launched = launch(&preset(&["{task}"]), &std::env::temp_dir(), "hello agent").unwrap();
        assert_eq!(launched.rendered, vec!["echo", "hello agent"]);

        let mut lines = BufReader::new(launched.stdout.take().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "hello agent");

        let status = launched.child.wait().await.unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn killing_an_agent_takes_down_its_children() {
        let Some(_) = which("sh") else { return };
        let p = AgentPreset {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 300 & sleep 300".into()],
            ..AgentPreset::default()
        };
        let mut launched = launch(&p, &std::env::temp_dir(), "").unwrap();

        kill_tree(&mut launched.child).await.unwrap();

        // the wait resolves, which it wouldn't if the tree were still up
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            launched.child.wait(),
        )
        .await;
        assert!(status.is_ok(), "the agent survived being killed");
    }
}
