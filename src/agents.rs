//! Spawning worker agents — some coding CLI (`claude`, `codex`, whatever you
//! configured) started in a directory with a task. We own its pipes: stdout and
//! stderr become session events, stdin stays open for follow-ups.
//!
//! A `pty` preset gets a real terminal instead, because a tui agent draws
//! nothing down a pipe; core turns those bytes into a screen. The trade is that
//! we hold the terminal, so those agents end when we do.
//!
//! Knows nothing about the registry or telegram on purpose: it launches
//! processes, core wires the output up.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

use crate::config::AgentPreset;
use crate::screen::{DEFAULT_COLS, DEFAULT_ROWS};

/// What `launch` hands back: a handle to wait on, and its io.
pub struct Launched {
    pub child: Handle,
    pub io: Io,
    /// The command line we actually ran, for the session log.
    pub rendered: Vec<String>,
}

/// The two shapes an agent's io comes in.
pub enum Io {
    Pipes {
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        stdin: Option<ChildStdin>,
    },
    /// One bidirectional stream, plus the master for resizes. Both ends block,
    /// so core keeps them off the runtime threads.
    Pty {
        master: Box<dyn MasterPty + Send>,
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
    },
}

/// How an agent finished. `code` is None when a signal ended it, same as
/// `std::process::ExitStatus::code`.
#[derive(Debug, Clone, Copy)]
pub struct Exit {
    pub code: Option<i32>,
}

impl Exit {
    #[allow(dead_code)]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// A running agent, whichever way it was started.
pub enum Handle {
    Piped(Child),
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}

impl Handle {
    pub fn id(&self) -> Option<u32> {
        match self {
            Handle::Piped(c) => c.id(),
            Handle::Pty(c) => c.process_id(),
        }
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<Exit>> {
        match self {
            Handle::Piped(c) => Ok(c.try_wait()?.map(|s| Exit { code: s.code() })),
            Handle::Pty(c) => Ok(c.try_wait()?.map(|s| Exit {
                code: (s.signal().is_none()).then(|| s.exit_code() as i32),
            })),
        }
    }

    /// Polled rather than blocked on, so nothing sits on a lock the killer
    /// needs.
    pub async fn wait(&mut self) -> std::io::Result<Exit> {
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Handle::Piped(c) => c.start_kill(),
            Handle::Pty(c) => c.kill(),
        }
    }
}

/// Substitute a preset's placeholders. An argument that is exactly `{task}` is
/// swapped whole, so spaces and quotes in the task stay one argument and never
/// meet a shell; embedded ones like `--message={task}` are substituted in place.
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

/// Find an executable on PATH, the way a shell would — so the picker shows only
/// what's installed, and a missing one gets a clear error.
pub fn which(command: &str) -> Option<PathBuf> {
    // an explicit path is used as given
    if command.contains('/') || command.contains('\\') {
        let p = PathBuf::from(command);
        return is_executable(&p).then_some(p);
    }

    let path = std::env::var_os("PATH")?;
    let extensions = candidate_extensions(cfg!(windows), std::env::var("PATHEXT").ok());

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

/// Suffixes to try after a bare command name. Always starts with the empty
/// string so the name is tried as given — `claude.cmd` already has its
/// extension and would never be found if we only appended.
///
/// Takes its inputs as arguments so the windows branch is testable anywhere.
fn candidate_extensions(windows: bool, pathext: Option<String>) -> Vec<String> {
    let mut all = vec![String::new()];
    if windows {
        all.extend(
            pathext
                .unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".into())
                .split(';')
                .filter(|e| !e.is_empty())
                .map(|e| e.to_lowercase()),
        );
    }
    all
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
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The agent picker the UI draws: every preset, plus whether its command is
/// actually installed here.
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
                "pty": preset.pty,
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

/// Expand `~` and make the path absolute, so a directory typed on a phone
/// resolves like it would in a shell.
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

/// Whether `dir` is inside one of the allowed roots. Gates spawning from
/// telegram only — reaching the local web UI already means being at the machine.
///
/// Both sides are canonicalized: resolving only the root compares paths that
/// went through different amounts of symlink resolution (macos `/var` →
/// `/private/var`, windows `\\?\` prefixes) and an allowed dir reads as denied.
pub fn dir_is_allowed(dir: &Path, allowed: &[PathBuf]) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
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
    let mut rendered = vec![preset.command.clone()];
    rendered.extend(args.clone());

    let (child, io) = if preset.pty {
        launch_on_pty(preset, &program, &args, dir)?
    } else {
        launch_on_pipes(preset, &program, &args, dir)?
    };

    Ok(Launched { child, io, rendered })
}

/// The pty path, for agents that draw a terminal ui. portable-pty gives the
/// child its own session with the pty as controlling terminal, so a group-wide
/// signal from the agent can't reach the daemon — same protection
/// `own_process_group` buys the pipe path, only stronger.
fn launch_on_pty(
    preset: &AgentPreset,
    program: &Path,
    args: &[String],
    dir: &Path,
) -> Result<(Handle, Io)> {
    let pty = portable_pty::native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening a pty for the agent")?;

    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    cmd.cwd(dir);
    // the whole point: a terminal the agent will actually draw on
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("NO_COLOR");
    // removals first, so a preset's own env can put one back deliberately
    for key in &preset.unset {
        cmd.env_remove(key);
    }
    for (key, value) in &preset.env {
        cmd.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("starting {}", program.display()))?;
    // the slave fd has to go, or the pty never reports eof when the agent exits
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().context("reading the agent's pty")?;
    let writer = pair.master.take_writer().context("writing to the agent's pty")?;

    Ok((
        Handle::Pty(child),
        Io::Pty { master: pair.master, reader, writer },
    ))
}

fn launch_on_pipes(
    preset: &AgentPreset,
    program: &Path,
    args: &[String],
    dir: &Path,
) -> Result<(Handle, Io)> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // agents that colour their output make a mess of the log pane
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");

    // deliberately *not* kill_on_drop: that hands an agent's life to whoever
    // holds the `Child`, so dropping the process table — or unwinding out of an
    // unrelated error — SIGKILLs every running agent with nothing to tell the
    // user why. agents end in one place, `kill_tree` from `kill_agent`.
    for key in &preset.unset {
        cmd.env_remove(key);
    }
    for (key, value) in &preset.env {
        cmd.env(key, value);
    }

    own_process_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;

    let io = Io::Pipes {
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        stdin: child.stdin.take(),
    };
    Ok((Handle::Piped(child), io))
}

/// Put a child in a process group of its own. Two things need it: killing the
/// child takes its whole tree (agents shell out constantly), and a group-wide
/// signal the child sends stops there instead of travelling up into our group
/// and hitting the daemon and every other agent.
pub fn own_process_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            setpgid_to_self();
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = cmd;
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

/// SIGTERM a whole process group, given its leader's pid. Doesn't wait — the
/// caller either has the `Child` or just wants the strays gone.
pub fn signal_process_group(pid: u32) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        // a negative pid means the group, which own_process_group made it lead
        unsafe {
            kill(-(pid as i32), SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Kill a whole process group on unix, or just the child elsewhere. Agents
/// shell out constantly, so signalling only the parent leaves builds running.
pub async fn kill_tree(child: &mut Handle) -> Result<()> {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        signal_process_group(pid);
        // give it a moment to go down politely before pulling the plug
        let graceful =
            tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
        if graceful.is_ok() {
            return Ok(());
        }
    }

    child.kill().context("killing the agent process")?;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
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
    fn the_bare_name_is_always_tried_first() {
        // regression: the windows list was built purely by appending PATHEXT
        // entries, so a command like `claude.cmd` was never probed as given
        let windows = candidate_extensions(true, Some(".EXE;.CMD".into()));
        assert_eq!(windows, vec!["", ".exe", ".cmd"]);

        // empty entries in PATHEXT shouldn't produce duplicate bare probes
        let padded = candidate_extensions(true, Some(".EXE;;.BAT;".into()));
        assert_eq!(padded, vec!["", ".exe", ".bat"]);

        // and unix only ever wants the name itself
        assert_eq!(candidate_extensions(false, None), vec![""]);
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

    // the case that broke ci: macos hands out a `/var/…` temp dir that is really
    // `/private/var/…`, so a dir and root that agree disagree once one is
    // resolved. a symlinked root reproduces it anywhere.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_still_covers_what_is_under_it() {
        let base = std::env::temp_dir().join("telepager-symlink-test");
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        std::fs::create_dir_all(real.join("project")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let by_link = std::slice::from_ref(&link);
        let by_real = std::slice::from_ref(&real);

        // allowed by its unresolved name, entered by its unresolved name
        assert!(dir_is_allowed(&link.join("project"), by_link));
        // allowed by one name, entered by the other, in both directions
        assert!(dir_is_allowed(&link.join("project"), by_real));
        assert!(dir_is_allowed(&real.join("project"), by_link));
        // and a sibling of the real root is still out
        assert!(!dir_is_allowed(&base, by_link));

        std::fs::remove_dir_all(&base).unwrap();
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

        let Io::Pipes { stdout, .. } = &mut launched.io else {
            panic!("a non-interactive preset should not get a pty");
        };
        let mut lines = BufReader::new(stdout.take().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "hello agent");

        let status = launched.child.wait().await.unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_interactive_preset_gets_a_real_terminal() {
        let Some(_) = which("sh") else { return };
        let p = AgentPreset {
            command: "sh".into(),
            // both of these only answer the way we want on a tty
            args: vec!["-c".into(), "tty > /dev/null && echo yes; echo $TERM".into()],
            interactive: true,
            pty: true,
            ..AgentPreset::default()
        };
        let mut launched = launch(&p, &std::env::temp_dir(), "").unwrap();

        let Io::Pty { reader, .. } = &mut launched.io else {
            panic!("an interactive preset should get a pty");
        };
        let mut reader = std::mem::replace(reader, Box::new(std::io::empty()));
        let text = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
        .await
        .unwrap();

        assert!(text.contains("yes"), "the agent was not on a tty: {text:?}");
        assert!(text.contains("xterm-256color"), "TERM was not set: {text:?}");
        launched.child.wait().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_preset_can_take_a_variable_away_from_its_agent() {
        let Some(_) = which("sh") else { return };
        // an inherited variable the test can be sure of, rather than one it sets
        let Ok(_) = std::env::var("HOME") else { return };

        let mut env = std::collections::BTreeMap::new();
        env.insert("TELEPAGER_KEPT".to_string(), "yes".to_string());
        let p = AgentPreset {
            command: "sh".into(),
            args: vec!["-c".into(), "echo [${HOME:-gone}] [$TELEPAGER_KEPT]".into()],
            unset: vec!["HOME".into()],
            env,
            interactive: true,
            pty: true,
            ..AgentPreset::default()
        };
        let mut launched = launch(&p, &std::env::temp_dir(), "").unwrap();

        let Io::Pty { reader, .. } = &mut launched.io else {
            panic!("a pty preset should get a pty");
        };
        let mut reader = std::mem::replace(reader, Box::new(std::io::empty()));
        let text = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
        .await
        .unwrap();

        assert!(text.contains("[gone]"), "HOME survived: {text:?}");
        assert!(text.contains("[yes]"), "the preset's own env did not: {text:?}");
        launched.child.wait().await.unwrap();
    }

    // a pty child gets its own session, so a signal it sends its group can't
    // travel up into the daemon's
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pty_agent_leads_its_own_process_group() {
        let Some(_) = which("sh") else { return };
        let p = AgentPreset {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            interactive: true,
            pty: true,
            ..AgentPreset::default()
        };
        let mut launched = launch(&p, &std::env::temp_dir(), "").unwrap();
        let pid = launched.child.id().unwrap() as i32;

        unsafe extern "C" {
            fn getpgid(pid: i32) -> i32;
        }
        assert_eq!(unsafe { getpgid(pid) }, pid, "the agent joined our group");
        assert_ne!(pid, unsafe { getpgid(0) });

        kill_tree(&mut launched.child).await.unwrap();
    }

    /// A preset whose agent keeps writing to `marker`, so a test can tell
    /// "still running" from "killed" — `kill(pid, 0)` can't, an unreaped corpse
    /// still answers.
    #[cfg(unix)]
    fn ticker(marker: &Path) -> AgentPreset {
        AgentPreset {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                format!("i=0; while [ $i -lt 60 ]; do echo $i >> {}; i=$((i+1)); sleep 1; done", marker.display()),
            ],
            ..AgentPreset::default()
        }
    }

    #[cfg(unix)]
    fn ticks(marker: &Path) -> usize {
        std::fs::read_to_string(marker).map(|t| t.lines().count()).unwrap_or(0)
    }

    // regression: launch() armed kill_on_drop(true), so dropping the process
    // table — or unwinding out of an unrelated error — SIGKILLed every running
    // agent, with nothing to tell the user why.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_handle_does_not_kill_a_running_agent() {
        let Some(_) = which("sh") else { return };
        let dir = std::env::temp_dir().join(format!("telepager-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ticks");

        let launched = launch(&ticker(&marker), &dir, "").unwrap();
        let pid = launched.child.id().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let before = ticks(&marker);
        assert!(before > 0, "the agent never started");

        drop(launched);

        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert!(
            ticks(&marker) > before,
            "the agent stopped ticking once we dropped its handle"
        );

        signal_process_group(pid);
        let _ = std::fs::remove_dir_all(&dir);
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
