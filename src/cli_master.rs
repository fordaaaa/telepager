//! Running the master agent on a coding CLI instead of a raw model API.
//!
//! `claude` and `opencode` are already logged in, so pointing the master at one
//! means no api key anywhere. The agent loop then lives inside that CLI and
//! reaches our tools over MCP through the `telepager master-mcp` bridge.
//!
//! This file builds the command, runs it, and digs the reply and session id out
//! of whatever it printed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::agents;
use crate::config::{Config, Provider};
use crate::core::{Core, Fanout};
use crate::master::Origin;
use crate::session::EventKind;

/// A turn can involve several tool calls and a lot of thinking, but you're
/// waiting on it from a phone.
const TURN_TIMEOUT_SECONDS: u64 = 300;

/// How often the status line may be edited — telegram rate limits edits, and a
/// busy turn emits events faster than anyone reads.
const STATUS_TICK: Duration = Duration::from_millis(1200);

/// The MCP server name the CLI sees, and the prefix on its tools — which is why
/// `--allowedTools mcp__telepager` works.
const SERVER_NAME: &str = "telepager";

pub async fn reply(core: &Arc<Core>, cfg: &Config, text: &str, origin: Origin) -> Result<String> {
    let master = &cfg.master;
    let command = master.cli_command().context("no cli command for this provider")?;
    let program = agents::which(&command).ok_or_else(|| {
        anyhow::anyhow!(
            "'{command}' isn't installed, or isn't on telepager's PATH. \
             install it, or pick a different provider in the console."
        )
    })?;

    // the cli runs somewhere neutral: the master orchestrates, it doesn't edit
    let workdir = master_workdir()?;

    core.record(None, EventKind::Chat { role: "user".into(), text: text.to_string() })
        .await;

    let previous = {
        let mut conversation = core.master.lock().await;
        // the cli owns the real history; this copy is what the console draws
        conversation.push(crate::llm::Msg::User(text.to_string()));
        conversation.cli_session.clone()
    };
    let system = crate::master::system_prompt(master.system.as_deref());
    let exe = std::env::current_exe().context("finding our own binary")?;

    let plan = match master.provider {
        Provider::ClaudeCode => claude_plan(&exe, master, &system, text, previous.as_deref(), origin),
        Provider::Opencode => opencode_plan(&exe, master, &system, text, previous.as_deref(), origin),
        other => bail!("{} isn't a cli backend", other.as_str()),
    };

    log::debug!("master agent: {} {:?}", program.display(), plan.args);

    // say something before the cli has: on a phone a silent turn looks exactly
    // like the bot ignoring you. the console shows the conversation, so it
    // doesn't need this.
    let status = match origin {
        Origin::Telegram => core.thinking(None, "thinking…", &Fanout::default()).await.1,
        Origin::Ui => Fanout::default(),
    };

    // claude code streams its events, so the status line can follow along.
    // opencode only prints at the end, so there's nothing to follow.
    let (lines, narrator) = match (master.provider, status.is_empty()) {
        (Provider::ClaudeCode, false) => {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            (Some(tx), Some(tokio::spawn(narrate(core.clone(), status.clone(), rx))))
        }
        _ => (None, None),
    };

    let output = run_cli(&program, &plan, &workdir, Duration::from_secs(TURN_TIMEOUT_SECONDS), lines).await;

    // the status line is scaffolding — once the reply lands it's just noise
    let status = match narrator {
        Some(task) => task.await.unwrap_or(status),
        None => status,
    };
    core.erase(&status).await;

    let output = output.with_context(|| format!("running {command}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() && stdout.trim().is_empty() {
        bail!(
            "{command} exited with {}: {}",
            output.status.code().unwrap_or(-1),
            first_line(&stderr, 300)
        );
    }

    let parsed = match master.provider {
        // a cli that didn't stream still printed one result object
        Provider::ClaudeCode => parse_claude_stream(&stdout).unwrap_or_else(|| parse_claude(&stdout)),
        Provider::Opencode => parse_opencode(&stdout),
        _ => unreachable!("guarded above"),
    };

    if let Some(session) = parsed.session {
        core.master.lock().await.cli_session = Some(session);
    }

    let answer = if parsed.text.trim().is_empty() {
        // a cli that said nothing usually explained itself on stderr
        let hint = first_line(&stderr, 200);
        if hint.trim().is_empty() {
            format!("{command} finished without saying anything.")
        } else {
            format!("{command} finished without saying anything ({hint}).")
        }
    } else {
        parsed.text
    };

    core.master.lock().await.push(crate::llm::Msg::Assistant {
        text: answer.clone(),
        calls: Vec::new(),
    });
    core.record(None, EventKind::Chat { role: "master".into(), text: answer.clone() })
        .await;
    Ok(answer)
}

/// A command line and the environment it needs.
struct Plan {
    args: Vec<String>,
    env: Vec<(String, String)>,
    /// Variables to take away from the child before `env` is applied.
    unset: Vec<String>,
}

/// Build the master CLI's command without running it.
///
/// Its own process group, like a worker agent: left in ours it's a sibling of
/// the daemon, so a group-wide signal it sends — how a node cli usually takes
/// its tool subprocesses down at the end of a turn — hits the daemon and every
/// agent too.
fn build_command(program: &Path, plan: &Plan, workdir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&plan.args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .kill_on_drop(true);
    // removals first, so a plan can put something back deliberately
    for key in &plan.unset {
        cmd.env_remove(key);
    }
    for (key, value) in &plan.env {
        cmd.env(key, value);
    }
    agents::own_process_group(&mut cmd);
    cmd
}

/// Run one turn of the master CLI, giving up after `limit`.
///
/// On a timeout the whole group goes, not just the CLI: `kill_on_drop` reaches
/// only the process we spawned, and by then it has children — at minimum a
/// `master-mcp` bridge holding a connection to us — that would be orphaned for
/// good, a fresh set per overrun turn.
///
/// With a `progress` channel, stdout lines are handed over as they arrive;
/// without one this just collects at the end.
async fn run_cli(
    program: &Path,
    plan: &Plan,
    workdir: &Path,
    limit: Duration,
    progress: Option<tokio::sync::mpsc::Sender<String>>,
) -> Result<std::process::Output> {
    let child = build_command(program, plan, workdir)
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    let pid = child.id();

    let mut run = std::pin::pin!(drive(child, progress));
    tokio::select! {
        finished = &mut run => finished.context("waiting for the master agent"),
        _ = tokio::time::sleep(limit) => {
            if let Some(pid) = pid {
                agents::signal_process_group(pid);
            }
            bail!("it didn't answer within {}s", limit.as_secs())
        }
    }
}

/// Wait for the CLI, forwarding stdout lines to whoever is watching.
async fn drive(
    mut child: tokio::process::Child,
    progress: Option<tokio::sync::mpsc::Sender<String>>,
) -> std::io::Result<std::process::Output> {
    let Some(progress) = progress else {
        return child.wait_with_output().await;
    };

    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // stderr has to drain as well, or a chatty cli fills its pipe and stops
    let errors = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await.ok();
        buf
    });

    let mut collected = Vec::new();
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        // a full channel means the narrator is behind, and a skipped status
        // line matters less than holding the cli up
        let _ = progress.try_send(line.clone());
        collected.extend_from_slice(line.as_bytes());
        collected.push(b'\n');
    }

    let status = child.wait().await?;
    Ok(std::process::Output {
        status,
        stdout: collected,
        stderr: errors.await.unwrap_or_default(),
    })
}

/// Keep a status line up to date from the CLI's event stream, handing it back so
/// the caller can take it down.
async fn narrate(
    core: Arc<Core>,
    mut status: crate::core::Fanout,
    mut lines: tokio::sync::mpsc::Receiver<String>,
) -> crate::core::Fanout {
    let mut pending: Option<String> = None;
    let mut last = Instant::now();

    loop {
        match tokio::time::timeout(STATUS_TICK, lines.recv()).await {
            Ok(Some(line)) => pending = stream_status(&line).or(pending),
            Ok(None) => break,
            // nothing new to say; fall through and see if something is owed
            Err(_) => {}
        }

        if last.elapsed() >= STATUS_TICK {
            if let Some(text) = pending.take() {
                let (_, sent) = core.thinking(None, &text, &status).await;
                status = sent;
                last = Instant::now();
            }
        }
    }
    status
}

/// What one Claude Code stream event says the master is up to, if anything.
fn stream_status(line: &str) -> Option<String> {
    let event: Value = serde_json::from_str(line.trim()).ok()?;
    if event["type"] != "assistant" {
        return None;
    }

    let mut latest = None;
    for part in event["message"]["content"].as_array()? {
        match part["type"].as_str() {
            Some("tool_use") => {
                latest = Some(format!("calling {}", part["name"].as_str().unwrap_or("a tool")))
            }
            Some("text") => {
                let text = part["text"].as_str().unwrap_or("").trim();
                if !text.is_empty() {
                    latest = Some(first_line(text, 120));
                }
            }
            _ => {}
        }
    }
    latest
}

/// The MCP server definition handed to Claude Code.
fn claude_mcp_config(exe: &Path, origin: Origin) -> String {
    json!({
        "mcpServers": {
            SERVER_NAME: {
                "command": exe.display().to_string(),
                "args": ["master-mcp"],
                "env": { "TELEPAGER_ORIGIN": origin.as_str() },
            }
        }
    })
    .to_string()
}

fn claude_plan(
    exe: &Path,
    master: &crate::config::MasterConfig,
    system: &str,
    text: &str,
    previous: Option<&str>,
    origin: Origin,
) -> Plan {
    let mut args: Vec<String> = vec![
        "-p".into(),
        text.into(),
        // events as they happen, so the status line can follow the turn.
        // stream-json needs --verbose or the cli refuses it.
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        // only our tools: the master orchestrates, it doesn't edit code itself
        "--mcp-config".into(),
        claude_mcp_config(exe, origin),
        "--strict-mcp-config".into(),
        "--allowedTools".into(),
        format!("mcp__{SERVER_NAME}"),
        "--append-system-prompt".into(),
        system.into(),
    ];

    let model = master.model.trim();
    if !model.is_empty() {
        args.push("--model".into());
        args.push(model.into());
    }

    // resuming keeps the conversation going without us replaying history
    if let Some(session) = previous {
        args.push("--resume".into());
        args.push(session.into());
    }

    args.extend(master.args.iter().cloned());

    // claude code takes an api key out of the environment over the login you
    // did in the terminal, and headless it never says so. picking this
    // provider means "use my login", so a key only rides along when the config
    // asked for one by name.
    let (env, unset) = match named_key(master) {
        Some(key) => (vec![("ANTHROPIC_API_KEY".to_string(), key)], Vec::new()),
        None => (
            Vec::new(),
            vec!["ANTHROPIC_API_KEY".to_string(), "ANTHROPIC_AUTH_TOKEN".to_string()],
        ),
    };

    Plan { args, env, unset }
}

/// A key the config actually asked for, rather than one that happens to be
/// exported. Only the first kind should reach a cli that has its own login.
fn named_key(master: &crate::config::MasterConfig) -> Option<String> {
    if let Some(key) = master.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        return Some(key.to_string());
    }
    let name = master.api_key_env.as_deref().map(str::trim).filter(|n| !n.is_empty())?;
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn opencode_plan(
    exe: &Path,
    master: &crate::config::MasterConfig,
    system: &str,
    text: &str,
    previous: Option<&str>,
    origin: Origin,
) -> Plan {
    // opencode takes its whole config as inline json, so telepager never has
    // to write to the user's opencode.json
    let config = json!({
        "mcp": {
            SERVER_NAME: {
                "type": "local",
                "command": [exe.display().to_string(), "master-mcp"],
                "enabled": true,
                "environment": { "TELEPAGER_ORIGIN": origin.as_str() },
            }
        }
    })
    .to_string();

    let mut args: Vec<String> = vec![
        "run".into(),
        "--format".into(),
        "json".into(),
        // our mcp tools are the only ones it needs approval for
        "--auto".into(),
    ];

    let model = master.model.trim();
    if !model.is_empty() {
        args.push("--model".into());
        args.push(model.into());
    }

    let message = match previous {
        Some(session) => {
            args.push("--session".into());
            args.push(session.into());
            text.to_string()
        }
        // opencode has no system-prompt flag, so the instructions ride along
        // with the first message of a conversation
        None => format!("{system}\n\n---\n\n{text}"),
    };

    args.extend(master.args.iter().cloned());
    args.push(message);

    Plan {
        args,
        env: vec![("OPENCODE_CONFIG_CONTENT".into(), config)],
        // opencode carries its own logins, and none of them are ours to strip
        unset: Vec::new(),
    }
}

#[derive(Debug, Default, PartialEq)]
struct Parsed {
    text: String,
    session: Option<String>,
}

/// `claude -p --output-format json` prints one object with the whole result.
fn parse_claude(stdout: &str) -> Parsed {
    let Ok(value) = serde_json::from_str::<Value>(stdout.trim()) else {
        // not json after all — better to show the raw text than nothing
        return Parsed { text: stdout.trim().to_string(), session: None };
    };

    let text = value["result"]
        .as_str()
        .or_else(|| value["text"].as_str())
        .unwrap_or_default()
        .to_string();

    Parsed {
        text,
        session: value["session_id"].as_str().map(str::to_string),
    }
}

/// `claude --output-format stream-json` prints one event per line, ending with
/// the same result object plain json gives. `None` means it wasn't a stream, so
/// the caller can fall back.
fn parse_claude_stream(stdout: &str) -> Option<Parsed> {
    let mut parsed = Parsed::default();
    let mut assistant = String::new();
    let mut events = 0;

    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        events += 1;

        if let Some(id) = event["session_id"].as_str() {
            parsed.session = Some(id.to_string());
        }

        match event["type"].as_str() {
            Some("result") => {
                if let Some(text) = event["result"].as_str() {
                    parsed.text = text.to_string();
                }
            }
            // what it said along the way, in case there's no result event
            Some("assistant") => {
                for part in event["message"]["content"].as_array().into_iter().flatten() {
                    if let Some(text) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                        if !assistant.is_empty() {
                            assistant.push('\n');
                        }
                        assistant.push_str(text.trim());
                    }
                }
            }
            _ => {}
        }
    }

    if events == 0 {
        return None;
    }
    if parsed.text.trim().is_empty() {
        parsed.text = assistant;
    }
    Some(parsed)
}

/// `opencode run --format json` prints one event per line. The reply is every
/// text part joined; the session id is on every event.
fn parse_opencode(stdout: &str) -> Parsed {
    let mut text = String::new();
    let mut session = None;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if session.is_none() {
            session = event["sessionID"]
                .as_str()
                .or_else(|| event["part"]["sessionID"].as_str())
                .map(str::to_string);
        }

        if event["type"] == "text" {
            if let Some(part) = event["part"]["text"].as_str() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
        }
    }

    Parsed { text: text.trim().to_string(), session }
}

/// Somewhere neutral to run: not a project directory, so it can't wander into
/// editing one, and stable so `--resume` keeps working.
fn master_workdir() -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("telepager").join("master");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn first_line(text: &str, limit: usize) -> String {
    let flat = text.trim().replace('\n', " ");
    if flat.chars().count() <= limit {
        return flat;
    }
    flat.chars().take(limit).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MasterConfig;

    fn master(provider: Provider) -> MasterConfig {
        MasterConfig { provider, ..MasterConfig::default() }
    }

    fn exe() -> PathBuf {
        PathBuf::from("/usr/bin/telepager")
    }

    fn arg_after(args: &[String], flag: &str) -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).cloned()
    }

    #[test]
    fn claude_gets_our_mcp_server_and_nothing_else() {
        let plan = claude_plan(&exe(), &master(Provider::ClaudeCode), "SYS", "do it", None, Origin::Ui);

        assert!(plan.args.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(arg_after(&plan.args, "--allowedTools").unwrap(), "mcp__telepager");
        assert_eq!(arg_after(&plan.args, "--append-system-prompt").unwrap(), "SYS");
        assert_eq!(arg_after(&plan.args, "--output-format").unwrap(), "stream-json");
        assert!(plan.args.contains(&"--verbose".to_string()));

        let config: Value = serde_json::from_str(&arg_after(&plan.args, "--mcp-config").unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["telepager"]["args"][0], "master-mcp");
        assert_eq!(config["mcpServers"]["telepager"]["env"]["TELEPAGER_ORIGIN"], "ui");

        // the message is an argument of its own, never shell-interpolated
        assert!(plan.args.contains(&"do it".to_string()));
        // no session to resume on the first turn
        assert!(!plan.args.contains(&"--resume".to_string()));
    }

    #[test]
    fn claude_resumes_an_existing_conversation() {
        let plan = claude_plan(&exe(), &master(Provider::ClaudeCode), "SYS", "and now?", Some("sess-1"), Origin::Ui);
        assert_eq!(arg_after(&plan.args, "--resume").unwrap(), "sess-1");
    }

    #[test]
    fn telegram_origin_reaches_the_bridge() {
        let plan = claude_plan(&exe(), &master(Provider::ClaudeCode), "s", "t", None, Origin::Telegram);
        let config: Value = serde_json::from_str(&arg_after(&plan.args, "--mcp-config").unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["telepager"]["env"]["TELEPAGER_ORIGIN"], "telegram");

        let oc = opencode_plan(&exe(), &master(Provider::Opencode), "s", "t", None, Origin::Telegram);
        let config: Value = serde_json::from_str(&oc.env[0].1).unwrap();
        assert_eq!(config["mcp"]["telepager"]["environment"]["TELEPAGER_ORIGIN"], "telegram");
    }

    #[test]
    fn a_model_is_only_passed_when_one_is_set() {
        let plan = claude_plan(&exe(), &master(Provider::ClaudeCode), "s", "t", None, Origin::Ui);
        assert!(arg_after(&plan.args, "--model").is_none());

        let picked = MasterConfig { model: "opus".into(), ..master(Provider::ClaudeCode) };
        let plan = claude_plan(&exe(), &picked, "s", "t", None, Origin::Ui);
        assert_eq!(arg_after(&plan.args, "--model").unwrap(), "opus");
    }

    #[test]
    fn an_exported_api_key_does_not_reach_claude_code() {
        // the whole point of this backend is the login, and claude code would
        // quietly prefer the key
        let plan = claude_plan(&exe(), &master(Provider::ClaudeCode), "s", "t", None, Origin::Ui);
        assert!(plan.unset.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(plan.unset.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
        assert!(plan.env.is_empty());
    }

    #[test]
    fn a_key_the_config_asked_for_is_handed_over() {
        let with_key = MasterConfig {
            api_key: Some("sk-test".into()),
            ..master(Provider::ClaudeCode)
        };
        let plan = claude_plan(&exe(), &with_key, "s", "t", None, Origin::Ui);
        assert_eq!(plan.env, vec![("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string())]);
        assert!(plan.unset.is_empty());
    }

    #[test]
    fn opencode_carries_its_config_inline_so_the_users_file_is_untouched() {
        let plan = opencode_plan(&exe(), &master(Provider::Opencode), "SYS", "do it", None, Origin::Ui);

        assert_eq!(plan.env.len(), 1);
        assert_eq!(plan.env[0].0, "OPENCODE_CONFIG_CONTENT");
        let config: Value = serde_json::from_str(&plan.env[0].1).unwrap();
        assert_eq!(config["mcp"]["telepager"]["type"], "local");
        assert_eq!(config["mcp"]["telepager"]["command"][1], "master-mcp");
        assert_eq!(config["mcp"]["telepager"]["enabled"], true);

        assert_eq!(plan.args[0], "run");
        assert!(plan.args.contains(&"--auto".to_string()));
        // no system-prompt flag, so it rides with the first message
        let message = plan.args.last().unwrap();
        assert!(message.starts_with("SYS"));
        assert!(message.ends_with("do it"));
    }

    #[test]
    fn opencode_stops_resending_the_system_prompt_once_it_has_a_session() {
        let plan = opencode_plan(&exe(), &master(Provider::Opencode), "SYS", "next", Some("ses_1"), Origin::Ui);
        assert_eq!(arg_after(&plan.args, "--session").unwrap(), "ses_1");
        assert_eq!(plan.args.last().unwrap(), "next");
    }

    #[test]
    fn extra_configured_args_are_appended() {
        let cfg = MasterConfig { args: vec!["--verbose".into()], ..master(Provider::ClaudeCode) };
        let plan = claude_plan(&exe(), &cfg, "s", "t", None, Origin::Ui);
        assert!(plan.args.contains(&"--verbose".to_string()));
    }

    #[test]
    fn claude_json_gives_up_its_reply_and_session() {
        let parsed = parse_claude(
            r#"{"type":"result","subtype":"success","is_error":false,
                "result":"Started claude in /tmp.","session_id":"abc-123"}"#,
        );
        assert_eq!(parsed.text, "Started claude in /tmp.");
        assert_eq!(parsed.session.as_deref(), Some("abc-123"));
    }

    #[test]
    fn a_claude_stream_gives_up_its_reply_and_session() {
        let stdout = concat!(
            r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#, "\n",
            r#"{"type":"assistant","session_id":"abc-123","message":{"content":[{"type":"tool_use","name":"mcp__telepager__spawn_agent"}]}}"#, "\n",
            r#"{"type":"result","subtype":"success","result":"Started claude in /tmp.","session_id":"abc-123"}"#, "\n",
        );
        let parsed = parse_claude_stream(stdout).unwrap();
        assert_eq!(parsed.text, "Started claude in /tmp.");
        assert_eq!(parsed.session.as_deref(), Some("abc-123"));
    }

    #[test]
    fn a_stream_that_never_gets_a_result_falls_back_to_what_it_said() {
        let stdout = concat!(
            r#"{"type":"assistant","session_id":"s1","message":{"content":[{"type":"text","text":"Started it."}]}}"#, "\n",
            r#"{"type":"assistant","session_id":"s1","message":{"content":[{"type":"text","text":"Session s3."}]}}"#, "\n",
        );
        let parsed = parse_claude_stream(stdout).unwrap();
        assert_eq!(parsed.text, "Started it.\nSession s3.");
        assert_eq!(parsed.session.as_deref(), Some("s1"));
    }

    #[test]
    fn output_that_never_streamed_falls_back_to_the_plain_parse() {
        // a cli that ignored --output-format, so there's nothing to stream
        assert!(parse_claude_stream("something went sideways").is_none());
        // and the plain result object still parses either way
        let one = r#"{"type":"result","result":"done","session_id":"s2"}"#;
        assert_eq!(parse_claude_stream(one).unwrap(), parse_claude(one));
    }

    #[test]
    fn the_status_line_says_what_the_master_is_doing() {
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__telepager__spawn_agent"}]}}"#;
        assert_eq!(stream_status(tool).unwrap(), "calling mcp__telepager__spawn_agent");

        let talking = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking at the tests"}]}}"#;
        assert_eq!(stream_status(talking).unwrap(), "Looking at the tests");

        // the rest of the stream has nothing to say
        assert!(stream_status(r#"{"type":"result","result":"done"}"#).is_none());
        assert!(stream_status("not json").is_none());
    }

    #[test]
    fn claude_output_that_isnt_json_is_shown_rather_than_swallowed() {
        let parsed = parse_claude("something went sideways");
        assert_eq!(parsed.text, "something went sideways");
        assert!(parsed.session.is_none());
    }

    #[test]
    fn opencode_events_are_joined_into_one_reply() {
        // the real shape, from `opencode run --format json`
        let stdout = concat!(
            r#"{"type":"step_start","sessionID":"ses_9","part":{"type":"step-start"}}"#, "\n",
            r#"{"type":"text","sessionID":"ses_9","part":{"type":"text","text":"Started it."}}"#, "\n",
            r#"{"type":"text","sessionID":"ses_9","part":{"type":"text","text":"Session s3."}}"#, "\n",
            r#"{"type":"step_finish","sessionID":"ses_9","part":{"type":"step-finish"}}"#, "\n",
        );
        let parsed = parse_opencode(stdout);
        assert_eq!(parsed.text, "Started it.\nSession s3.");
        assert_eq!(parsed.session.as_deref(), Some("ses_9"));
    }

    #[test]
    fn a_broken_line_does_not_lose_the_rest_of_the_stream() {
        let stdout = concat!(
            "not json at all\n",
            r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"still here"}}"#, "\n",
        );
        let parsed = parse_opencode(stdout);
        assert_eq!(parsed.text, "still here");
        assert_eq!(parsed.session.as_deref(), Some("ses_1"));
    }

    #[test]
    fn empty_output_parses_to_nothing_rather_than_panicking() {
        assert_eq!(parse_opencode(""), Parsed::default());
        assert_eq!(parse_claude("").text, "");
    }

    #[cfg(unix)]
    fn shell_plan(script: &str) -> Plan {
        Plan { args: vec!["-c".into(), script.into()], env: Vec::new(), unset: Vec::new() }
    }

    #[cfg(unix)]
    fn our_process_group() -> u32 {
        unsafe extern "C" {
            fn getpgrp() -> i32;
        }
        unsafe { getpgrp() as u32 }
    }

    // regression: the master cli was left in telepager's process group, so a
    // group-wide signal from it — how a node cli takes its tool subprocesses
    // down at the end of a turn — reached the daemon and every agent.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_master_cli_runs_in_a_process_group_of_its_own() {
        let Some(sh) = agents::which("sh") else { return };
        let out = run_cli(
            &sh,
            &shell_plan("ps -o pgid= -p $$"),
            &std::env::temp_dir(),
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap();

        let printed: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert_ne!(printed, our_process_group(), "the cli shares telepager's group");
    }

    // regression: an overrunning turn was abandoned with only kill_on_drop,
    // which SIGKILLs the cli and nothing else — its `master-mcp` bridge and
    // anything else it started were orphaned for good, one set per overrun.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_that_overruns_takes_the_clis_children_with_it() {
        let Some(sh) = agents::which("sh") else { return };
        let dir = std::env::temp_dir().join(format!("telepager-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ticks");

        // a stand-in for the mcp bridge: a grandchild that outlives its parent
        let script = format!(
            "(i=0; while [ $i -lt 60 ]; do echo $i >> {}; i=$((i+1)); sleep 1; done) & sleep 60",
            marker.display()
        );
        let err = run_cli(&sh, &shell_plan(&script), &dir, Duration::from_millis(1500), None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("didn't answer"), "{err:#}");

        let ticks = || std::fs::read_to_string(&marker).map(|t| t.lines().count()).unwrap_or(0);
        let before = ticks();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(ticks(), before, "the cli's children outlived the turn");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_master_workdir_is_not_a_project_directory() {
        let dir = master_workdir().unwrap();
        assert!(dir.ends_with("telepager/master"));
        assert!(dir.is_dir());
    }
}
