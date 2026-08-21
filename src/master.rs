//! The master agent: the thing you talk to.
//!
//! You message it without addressing any particular session, and it decides
//! what to do — start a worker somewhere, tell you what the running ones are
//! doing, kill one that's stuck, answer a question a worker is blocked on.
//!
//! It is a client of core like the UI is, with no privileges the UI doesn't
//! have. Routing an answer back to the session that asked stays a hashmap
//! lookup in core; putting a model in that path would only add latency and a
//! new way to be wrong.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::core::{Answer, Core};
use crate::llm::{Llm, Msg, ToolCall, ToolDef, Turn};
use crate::session::EventKind;

/// How many tool round trips one message may take before we stop and answer
/// with what we have. Stops a confused model looping forever on your bill.
const MAX_STEPS: usize = 8;
/// How much conversation to carry. Old turns are dropped from the front.
const MAX_HISTORY: usize = 60;

/// Where a message came from, which decides what spawning is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The local web UI. Reaching it means being at the machine.
    Ui,
    /// Telegram, which is remote and only reaches allowlisted directories.
    Telegram,
}

impl Origin {
    fn is_telegram(self) -> bool {
        matches!(self, Origin::Telegram)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Ui => "ui",
            Origin::Telegram => "telegram",
        }
    }

    /// Only an explicit "ui" is treated as local. Telegram is the gated
    /// surface, so it's the safe thing to assume when the origin is missing
    /// or malformed — the callers that are genuinely local always say so.
    pub fn parse(s: &str) -> Origin {
        match s {
            "ui" => Origin::Ui,
            _ => Origin::Telegram,
        }
    }
}

#[derive(Default)]
pub struct Conversation {
    pub history: Vec<Msg>,
    /// A CLI backend keeps the real conversation itself; this is the id we
    /// resume it by. `history` is then only what the console draws.
    pub cli_session: Option<String>,
}

impl Conversation {
    pub fn push(&mut self, msg: Msg) {
        self.history.push(msg);
        if self.history.len() > MAX_HISTORY {
            let drop = self.history.len() - MAX_HISTORY;
            self.history.drain(..drop);

            // a conversation has to start on a user turn: anthropic rejects
            // anything else outright, and gemini's contents are turn-ordered.
            // a tool result whose call got trimmed is just as broken.
            while !matches!(self.history.first(), None | Some(Msg::User(_))) {
                self.history.remove(0);
            }
        }
    }

    pub fn clear(&mut self) {
        self.history.clear();
        // a new conversation means a new cli session, not a resumed one
        self.cli_session = None;
    }

    /// The conversation as the UI draws it.
    pub fn transcript(&self) -> Vec<Value> {
        self.history
            .iter()
            .filter_map(|m| match m {
                Msg::User(text) => Some(json!({ "role": "user", "text": text })),
                Msg::Assistant { text, calls } if !text.trim().is_empty() || !calls.is_empty() => {
                    Some(json!({
                        "role": "master",
                        "text": text,
                        "calls": calls.iter().map(|c| json!({
                            "name": c.name,
                            "args": c.args,
                        })).collect::<Vec<_>>(),
                    }))
                }
                Msg::ToolResult { name, content, .. } => Some(json!({
                    "role": "tool",
                    "name": name,
                    "text": content,
                })),
                _ => None,
            })
            .collect()
    }
}

pub(crate) fn system_prompt(extra: Option<&str>) -> String {
    let mut prompt = String::from(
        "You are the master agent inside telepager. The person you're talking to is \
         reaching you from their phone over Telegram, or from a small web console on \
         their own machine. They use you to run and watch coding agents on that \
         machine while they are away from it.\n\n\
         What you can do:\n\
         - Start a worker agent (claude, codex, gemini, opencode and friends) in a \
           directory with a task.\n\
         - See what every session is doing, read a worker's output, and summarise it.\n\
         - Type a follow-up at a running worker, or kill one that's stuck.\n\
         - Answer a question a worker is blocked on, but only when the person has \
           told you what they want; otherwise leave it for them.\n\n\
         How to behave:\n\
         - Be brief. Your replies are read on a phone, so a couple of sentences beats \
           a report. No markdown tables, no headings.\n\
         - Check before you guess. Call list_sessions rather than assuming what's \
           running, and read_output before you summarise a worker.\n\
         - Ask before doing something destructive or expensive. Starting a worker in \
           a directory they named is fine; killing something they didn't mention is not.\n\
         - When you start a worker, say which agent and which directory in your reply, \
           so they can catch a mistake.\n\
         - You cannot run shell commands yourself. If something needs a command run, \
           start a worker agent with that as its task.\n\
         - If a tool fails, say so plainly and what you'd try instead. Don't retry the \
           same call twice.",
    );
    if let Some(extra) = extra.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n\nFrom the person who set this up:\n");
        prompt.push_str(extra);
    }
    prompt
}

pub(crate) fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_sessions".into(),
            description: "List every session telepager knows about: attached MCP clients \
                          and workers you started, with their state and current task."
                .into(),
            schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "list_agents".into(),
            description: "List the worker agent CLIs available on this machine. Use it \
                          before spawning if you're not sure what's installed."
                .into(),
            schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "spawn_agent".into(),
            description: "Start a worker agent in a directory with a task. The task is \
                          the whole prompt the agent gets, so make it specific."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "which agent, e.g. claude or codex" },
                    "dir": { "type": "string", "description": "absolute path to work in" },
                    "task": { "type": "string", "description": "what the agent should do" },
                },
                "required": ["agent", "dir", "task"],
            }),
        },
        ToolDef {
            name: "read_output".into(),
            description: "Read the tail of a session's output.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "session id, e.g. s3" },
                    "lines": { "type": "integer", "description": "how many lines, default 60" },
                },
                "required": ["session"],
            }),
        },
        ToolDef {
            name: "send_to_agent".into(),
            description: "Type a line at a running worker's stdin.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "text": { "type": "string" },
                },
                "required": ["session", "text"],
            }),
        },
        ToolDef {
            name: "kill_agent".into(),
            description: "Stop a running worker and everything it started.".into(),
            schema: json!({
                "type": "object",
                "properties": { "session": { "type": "string" } },
                "required": ["session"],
            }),
        },
        ToolDef {
            name: "answer_question".into(),
            description: "Answer a question a session is blocked on. Only use this when \
                          the person has told you how they want it answered."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question_id": { "type": "string", "description": "from list_sessions" },
                    "answer": { "type": "string", "description": "one of the offered options, or free text" },
                },
                "required": ["question_id", "answer"],
            }),
        },
    ]
}

/// Handle one message to the master agent and return what to say back.
pub async fn reply(core: &Arc<Core>, text: &str, origin: Origin) -> Result<String> {
    let cfg = core.config().await.context("telepager isn't set up yet")?;

    // a cli backend (claude code, opencode) runs its own agent loop and reaches
    // our tools over mcp, so there's nothing for the http loop to do here
    if cfg.master.provider.is_cli() {
        return crate::cli_master::reply(core, &cfg, text, origin).await;
    }

    if !cfg.master.is_usable() {
        anyhow::bail!(
            "the master agent has no api key. set {} in your environment, or pick a \
             provider in the web console.",
            cfg.master.provider.default_key_env()
        );
    }
    let llm = Llm::new(&cfg.master)?;
    log::debug!("master agent replying via {}", llm.describe());
    let system = system_prompt(cfg.master.system.as_deref());
    let toolbox = tools();

    core.record(None, EventKind::Chat { role: "user".into(), text: text.to_string() })
        .await;
    core.master.lock().await.push(Msg::User(text.to_string()));

    let mut final_text = String::new();

    for step in 0..MAX_STEPS {
        let history = core.master.lock().await.history.clone();
        let turn: Turn = llm.turn(&system, &history, &toolbox).await?;

        // a turn with neither prose nor tool calls isn't a legal message for
        // any provider, and openai rejects the whole request over it
        if !turn.text.trim().is_empty() || !turn.calls.is_empty() {
            core.master.lock().await.push(Msg::Assistant {
                text: turn.text.clone(),
                calls: turn.calls.clone(),
            });
        }

        if !turn.text.trim().is_empty() {
            core.record(None, EventKind::Chat { role: "master".into(), text: turn.text.clone() })
                .await;
            final_text = turn.text.clone();
        }

        if turn.calls.is_empty() {
            break;
        }

        for call in &turn.calls {
            let result = run_tool(core, call, origin).await;
            core.record(
                None,
                EventKind::Chat {
                    role: "tool".into(),
                    text: format!("{} → {}", call.name, first_line(&result, 200)),
                },
            )
            .await;
            core.master.lock().await.push(Msg::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                content: result,
            });
        }

        if step == MAX_STEPS - 1 {
            final_text = if final_text.trim().is_empty() {
                "I got stuck going back and forth on that. Ask me again, more specifically?".into()
            } else {
                final_text
            };
        }
    }

    if final_text.trim().is_empty() {
        final_text = "Done.".into();
    }
    Ok(final_text)
}

/// Run one tool call and describe the outcome in a line or two of plain text,
/// which is what models act on most reliably.
pub(crate) async fn run_tool(core: &Arc<Core>, call: &ToolCall, origin: Origin) -> String {
    let args = &call.args;
    match call.name.as_str() {
        "list_sessions" => {
            let lines = core.session_lines().await;
            let pending = core.pending_questions().await;

            let mut out = if lines.is_empty() {
                "No sessions.".to_string()
            } else {
                lines.join("\n")
            };
            if !pending.is_empty() {
                out.push_str("\n\nWaiting on you:");
                for q in pending {
                    out.push_str(&format!(
                        "\n{} (session {}): {} — options: {}",
                        q["id"].as_str().unwrap_or("?"),
                        q["session"].as_str().unwrap_or("?"),
                        q["question"].as_str().unwrap_or("?"),
                        q["options"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|o| o.as_str()).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default()
                    ));
                }
            }
            out
        }

        "list_agents" => match core.config().await {
            Some(cfg) => {
                let installed: Vec<String> = crate::agents::catalog(&cfg.agents)
                    .into_iter()
                    .filter(|a| a["installed"].as_bool().unwrap_or(false))
                    .map(|a| {
                        format!(
                            "{} — {}",
                            a["name"].as_str().unwrap_or("?"),
                            a["description"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                if installed.is_empty() {
                    "No agent CLIs are installed on this machine.".into()
                } else {
                    installed.join("\n")
                }
            }
            None => "telepager isn't set up yet.".into(),
        },

        "spawn_agent" => {
            let agent = str_arg(args, "agent");
            let dir = str_arg(args, "dir");
            let task = str_arg(args, "task");
            match core.spawn_agent(&agent, &dir, &task, origin.is_telegram()).await {
                Ok(id) => format!("Started {agent} in {dir} as session {id}."),
                Err(e) => format!("Could not start it: {e:#}"),
            }
        }

        "read_output" => {
            let session = str_arg(args, "session");
            let lines = args["lines"].as_u64().unwrap_or(60).clamp(1, 400) as usize;
            match core.output_tail(&session, lines).await {
                Some(text) if text.trim().is_empty() => {
                    format!("{session} hasn't produced any output yet.")
                }
                Some(text) => text,
                None => format!("There's no session called {session}."),
            }
        }

        "send_to_agent" => {
            let session = str_arg(args, "session");
            let text = str_arg(args, "text");
            match core.write_to_agent(&session, &text).await {
                Ok(()) => format!("Sent it to {session}."),
                Err(e) => format!("Could not: {e:#}"),
            }
        }

        "kill_agent" => {
            let session = str_arg(args, "session");
            match core.kill_agent(&session).await {
                Ok(()) => format!("Killed {session}."),
                Err(e) => format!("Could not: {e:#}"),
            }
        }

        "answer_question" => {
            let qid = str_arg(args, "question_id");
            let answer = str_arg(args, "answer");
            if core.answer(&qid, Answer::Typed(answer.clone())).await {
                format!("Answered {qid} with: {answer}")
            } else {
                format!("{qid} isn't waiting for an answer any more.")
            }
        }

        other => format!("There's no tool called {other}."),
    }
}

fn str_arg(args: &Value, name: &str) -> String {
    args[name].as_str().unwrap_or_default().trim().to_string()
}

fn first_line(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= limit {
        return flat;
    }
    flat.chars().take(limit).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use std::path::PathBuf;

    fn core() -> Arc<Core> {
        Core::new(Some(PathBuf::from("/telepager-test/none.json")))
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall { id: "c1".into(), name: name.into(), args }
    }

    #[test]
    fn the_system_prompt_appends_rather_than_replaces() {
        let base = system_prompt(None);
        let with_extra = system_prompt(Some("always speak like a pirate"));
        assert!(with_extra.starts_with(&base));
        assert!(with_extra.contains("pirate"));
        // an empty personal note doesn't add a dangling header
        assert_eq!(system_prompt(Some("   ")), base);
    }

    #[test]
    fn every_tool_has_an_object_schema() {
        for t in tools() {
            assert_eq!(t.schema["type"], "object", "{} has a bad schema", t.name);
            assert!(!t.description.is_empty(), "{} has no description", t.name);
        }
    }

    #[test]
    fn trimming_always_leaves_a_user_turn_first() {
        let mut c = Conversation::default();
        // fill past the cap, ending on an assistant turn so the trim has to
        // walk past it to find a user message
        for i in 0..MAX_HISTORY {
            c.push(Msg::User(format!("m{i}")));
            c.push(Msg::Assistant { text: format!("r{i}"), calls: vec![] });
        }
        assert!(matches!(c.history.first(), Some(Msg::User(_))), "history must open on a user turn");
    }

    #[test]
    fn history_is_trimmed_without_orphaning_tool_results() {
        let mut c = Conversation::default();
        for i in 0..MAX_HISTORY {
            c.push(Msg::User(format!("message {i}")));
        }
        c.push(Msg::Assistant {
            text: String::new(),
            calls: vec![ToolCall { id: "x".into(), name: "t".into(), args: json!({}) }],
        });
        c.push(Msg::ToolResult { id: "x".into(), name: "t".into(), content: "done".into() });

        assert!(c.history.len() <= MAX_HISTORY);
        // a leading tool result would be rejected by every provider
        assert!(!matches!(c.history.first(), Some(Msg::ToolResult { .. })));
    }

    #[test]
    fn the_transcript_skips_empty_assistant_turns() {
        let mut c = Conversation::default();
        c.push(Msg::User("hi".into()));
        c.push(Msg::Assistant { text: String::new(), calls: vec![] });
        c.push(Msg::Assistant { text: "hello".into(), calls: vec![] });

        let t = c.transcript();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0]["role"], "user");
        assert_eq!(t[1]["text"], "hello");
    }

    #[test]
    fn clearing_a_conversation_empties_it() {
        let mut c = Conversation::default();
        c.push(Msg::User("hi".into()));
        c.clear();
        assert!(c.transcript().is_empty());
    }

    #[test]
    fn an_unknown_origin_falls_back_to_the_gated_one() {
        assert_eq!(Origin::parse("ui"), Origin::Ui);
        assert_eq!(Origin::parse("telegram"), Origin::Telegram);
        // anything else is assumed remote rather than trusted
        assert_eq!(Origin::parse(""), Origin::Telegram);
        assert_eq!(Origin::parse("nonsense"), Origin::Telegram);
        // and it round trips
        assert_eq!(Origin::parse(Origin::Ui.as_str()), Origin::Ui);
        assert_eq!(Origin::parse(Origin::Telegram.as_str()), Origin::Telegram);
    }

    #[tokio::test]
    async fn listing_sessions_with_none_says_so() {
        let c = core();
        let out = run_tool(&c, &call("list_sessions", json!({})), Origin::Ui).await;
        assert_eq!(out, "No sessions.");
    }

    #[tokio::test]
    async fn listing_sessions_includes_pending_questions() {
        let c = core();
        let id = c
            .open_session(crate::session::Kind::Attached, "proj".into(), None, None, None)
            .await;

        // ask parks a question; it returns once the timeout or an answer lands
        let asker = {
            let c = c.clone();
            let id = id.clone();
            tokio::spawn(async move { c.ask(&id, "ship it?", &["yes".into()]).await })
        };
        // unconfigured cores answer immediately, so this only checks the shape
        let _ = asker.await;

        let out = run_tool(&c, &call("list_sessions", json!({})), Origin::Ui).await;
        assert!(out.contains("proj"), "{out}");
    }

    #[tokio::test]
    async fn unknown_tools_and_sessions_are_reported_not_fatal() {
        let c = core();
        assert!(run_tool(&c, &call("nonsense", json!({})), Origin::Ui)
            .await
            .contains("no tool called"));
        assert!(run_tool(&c, &call("read_output", json!({ "session": "s99" })), Origin::Ui)
            .await
            .contains("no session"));
        assert!(run_tool(&c, &call("kill_agent", json!({ "session": "s99" })), Origin::Ui)
            .await
            .contains("Could not"));
        assert!(run_tool(&c, &call("answer_question", json!({ "question_id": "q9", "answer": "x" })), Origin::Ui)
            .await
            .contains("isn't waiting"));
    }

    #[tokio::test]
    async fn a_tool_call_with_missing_arguments_does_not_panic() {
        let c = core();
        let out = run_tool(&c, &call("spawn_agent", json!({})), Origin::Ui).await;
        assert!(out.contains("Could not start it"), "{out}");
    }

    #[tokio::test]
    async fn read_output_clamps_a_silly_line_count() {
        let c = core();
        let id = c
            .open_session(crate::session::Kind::Spawned, "x".into(), None, None, None)
            .await;
        let out = run_tool(
            &c,
            &call("read_output", json!({ "session": id, "lines": 999999 })),
            Origin::Ui,
        )
        .await;
        assert!(out.contains("hasn't produced any output"), "{out}");
    }

    #[tokio::test]
    async fn the_master_needs_a_config_before_it_will_talk() {
        let c = core();
        let err = reply(&c, "hello", Origin::Ui).await.unwrap_err();
        assert!(format!("{err:#}").contains("isn't set up"), "{err:#}");
    }
}
