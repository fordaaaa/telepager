//! Running the master agent on a coding CLI instead of a raw model API.
//!
//! `claude` and `opencode` are already logged in — a Claude Code subscription,
//! or opencode's own credentials including its free models — so pointing the
//! master agent at one means no API key anywhere. The agent loop then lives
//! inside that CLI, and it reaches telepager's tools over MCP, through the
//! `telepager master-mcp` bridge.
//!
//! What this file does: build the command, run it, and pull the reply and the
//! session id back out of whatever it printed.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::agents;
use crate::config::{Config, Provider};
use crate::core::Core;
use crate::master::Origin;
use crate::session::EventKind;

/// A master turn can involve several tool calls and a lot of thinking, but it
/// shouldn't run forever — you're waiting on it from a phone.
const TURN_TIMEOUT_SECONDS: u64 = 300;

/// The MCP server name the CLI sees. Also what its tools get prefixed with,
/// which is why `--allowedTools mcp__telepager` works.
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

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&plan.args)
        .current_dir(&workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .kill_on_drop(true);
    for (key, value) in &plan.env {
        cmd.env(key, value);
    }

    let run = cmd.output();
    let output = match tokio::time::timeout(Duration::from_secs(TURN_TIMEOUT_SECONDS), run).await {
        Ok(result) => result.with_context(|| format!("running {}", program.display()))?,
        Err(_) => bail!("{command} didn't answer within {TURN_TIMEOUT_SECONDS}s"),
    };

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
        Provider::ClaudeCode => parse_claude(&stdout),
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
}

/// The MCP server definition handed to Claude Code.
fn claude_mcp_config(exe: &PathBuf, origin: Origin) -> String {
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
    exe: &PathBuf,
    master: &crate::config::MasterConfig,
    system: &str,
    text: &str,
    previous: Option<&str>,
    origin: Origin,
) -> Plan {
    let mut args: Vec<String> = vec![
        "-p".into(),
        text.into(),
        "--output-format".into(),
        "json".into(),
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
    Plan { args, env: Vec::new() }
}

fn opencode_plan(
    exe: &PathBuf,
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

/// Somewhere neutral for the master to run: not a project directory, so it
/// can't wander into editing one, and stable so `--resume` keeps working.
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
        assert_eq!(arg_after(&plan.args, "--output-format").unwrap(), "json");

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

    #[test]
    fn the_master_workdir_is_not_a_project_directory() {
        let dir = master_workdir().unwrap();
        assert!(dir.ends_with("telepager/master"));
        assert!(dir.is_dir());
    }
}
