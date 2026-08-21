//! The background process everything else talks to.
//!
//! It runs three things over one [`Core`]: the local socket MCP shims connect
//! to, the Telegram poller, and the web console. Any of them can be the only
//! one in use — the app starts even with no Telegram token, because serving
//! its own setup page is how it gets one.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::core::{Answer, Core};
use crate::ipc::{self, Endpoint, Request, Response};
use crate::master::{self, Origin};
use crate::session::{Kind, State};
use crate::telegram::{IncomingMessage, Telegram};

pub fn run(config: Option<std::path::PathBuf>, open_browser: bool) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let core = Core::new(config);
        serve_all(core, open_browser).await
    })
}

pub async fn serve_all(core: Arc<Core>, open_browser: bool) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding the daemon socket")?;
    let port = listener.local_addr()?.port();
    let token = ipc::new_token();

    // the console comes up first so its url can go in the endpoint file
    let ui = crate::web::start(core.clone(), open_browser).await?;
    core.set_ui(ui.clone()).await;

    ipc::write_endpoint(&Endpoint {
        port,
        token: token.clone(),
        pid: std::process::id(),
        ui_port: Some(ui.port),
        ui_key: Some(ui.key.clone()),
    })?;
    log::info!("daemon listening on 127.0.0.1:{port}, console on {}", ui.url());

    tokio::spawn(telegram_loop(core.clone()));

    loop {
        let (stream, _) = listener.accept().await?;
        let core = core.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_client(core, stream, token).await {
                log::debug!("session ended: {e:#}");
            }
        });
    }
}

/// Poll Telegram for as long as there's a token, restarting when the config
/// changes underneath us — which is what makes the setup page take effect
/// without anyone restarting anything.
async fn telegram_loop(core: Arc<Core>) {
    loop {
        let Some(tg) = core.telegram().await else {
            // nothing to poll yet; wait for setup to write a token
            core.config_changed.notified().await;
            continue;
        };

        tokio::select! {
            _ = poll_updates(core.clone(), tg) => {}
            _ = core.config_changed.notified() => {
                log::info!("config changed, restarting the telegram poller");
            }
        }
    }
}

async fn poll_updates(core: Arc<Core>, tg: Arc<Telegram>) {
    let mut offset = 0i64;
    loop {
        let updates = match tg.get_updates(offset).await {
            Ok(u) => u,
            Err(e) => {
                log::debug!("getUpdates failed, retrying: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.update_id + 1;

            if let Some(msg) = update.message {
                handle_message(&core, msg).await;
                continue;
            }

            let Some(query) = update.callback_query else {
                continue;
            };
            if !allowed(&core, query.from.id).await {
                log::warn!("ignoring a button tap from {}", query.from.id);
                continue;
            }

            if let Err(e) = tg.answer_callback(&query.id).await {
                log::debug!("answerCallbackQuery failed: {e:#}");
            }

            let Some(data) = query.data.as_deref() else { continue };
            let Some((qid, index)) = parse_callback(data) else {
                log::debug!("unrecognised callback data: {data}");
                continue;
            };

            // buttons sent by an older telepager carry a bare index, so fall
            // back to the message the tap came from
            let qid = match core.question_for_message_or(&qid, query.message.as_ref().map(|m| m.message_id)).await {
                Some(id) => id,
                None => {
                    log::debug!("a tap arrived for {qid}, which is no longer waiting");
                    continue;
                }
            };
            if !core.answer(&qid, Answer::Tapped(index)).await {
                log::debug!("a tap arrived for {qid}, which is no longer waiting");
            }
        }
    }
}

/// Callback data is `<question id>:<option index>`.
fn parse_callback(data: &str) -> Option<(String, usize)> {
    let (qid, index) = data.rsplit_once(':')?;
    let index = index.parse().ok()?;
    (!qid.is_empty()).then(|| (qid.to_string(), index))
}

/// What a typed Telegram message should do.
enum Routing {
    /// Answer this waiting question.
    Answer(String),
    /// Hand it to the master agent.
    Master,
    Command(String),
}

async fn route_message(core: &Arc<Core>, msg: &IncomingMessage, text: &str) -> Routing {
    if let Some(rest) = text.strip_prefix('/') {
        return Routing::Command(rest.trim().to_string());
    }

    // replying to the question's own message is the unambiguous case
    if let Some(replied) = &msg.reply_to_message {
        if let Some(qid) = core.question_for_message(replied.message_id).await {
            return Routing::Answer(qid);
        }
    }

    // one question waiting and a bare message: they're answering it. this is
    // what telepager has always done and what a blocked agent needs.
    if let Some(qid) = core.only_pending_question().await {
        return Routing::Answer(qid);
    }

    Routing::Master
}

async fn handle_message(core: &Arc<Core>, msg: IncomingMessage) {
    let Some(from) = &msg.from else { return };
    if !allowed(core, from.id).await {
        log::warn!("ignoring a message from {}", from.id);
        return;
    }
    let Some(text) = msg.text.clone() else { return };
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }

    match route_message(core, &msg, &text).await {
        Routing::Answer(qid) => {
            if !core.answer(&qid, Answer::Typed(text.clone())).await {
                // it timed out between routing and delivery
                reply_to_telegram(core, "That question isn't waiting any more.").await;
            }
        }
        Routing::Command(command) => handle_command(core, &command).await,
        Routing::Master => {
            let answer = match master::reply(core, &text, Origin::Telegram).await {
                Ok(text) => text,
                Err(e) => format!("⚠️ {e:#}"),
            };
            reply_to_telegram(core, &answer).await;
        }
    }
}

async fn handle_command(core: &Arc<Core>, command: &str) {
    let (name, rest) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    let rest = rest.trim();

    let reply = match name {
        "start" | "help" => HELP.to_string(),
        "status" | "sessions" => {
            let lines = core.session_lines().await;
            if lines.is_empty() {
                "Nothing running.".to_string()
            } else {
                lines.join("\n")
            }
        }
        "new" | "reset" => {
            core.master.lock().await.clear();
            "Started a fresh conversation.".to_string()
        }
        "ui" | "webui" => match core.ui().await {
            Some(ui) => format!("The console is at {} (only reachable from that machine).", ui.url()),
            None => "The console isn't running.".to_string(),
        },
        "agents" => match core.config().await {
            Some(cfg) => {
                let installed: Vec<String> = crate::agents::catalog(&cfg.agents)
                    .into_iter()
                    .filter(|a| a["installed"].as_bool().unwrap_or(false))
                    .map(|a| format!("• {}", a["name"].as_str().unwrap_or("?")))
                    .collect();
                if installed.is_empty() {
                    "No agent CLIs are installed on that machine.".into()
                } else {
                    format!("Installed agents:\n{}", installed.join("\n"))
                }
            }
            None => "Not set up yet.".into(),
        },
        // anything else is just talk aimed at the master agent
        _ => match master::reply(core, command, Origin::Telegram).await {
            Ok(text) => text,
            Err(e) => format!("⚠️ {e:#}"),
        },
    };

    let _ = rest;
    reply_to_telegram(core, &reply).await;
}

const HELP: &str = "\
telepager — talk to me and I'll run agents on your machine.

Just say what you want, e.g. \"start claude in ~/code/api and make the tests pass\"
or \"what's running?\".

/status    what every session is doing
/agents    which agent CLIs are installed
/new       forget our conversation so far
/ui        the console's address
/help      this";

async fn reply_to_telegram(core: &Arc<Core>, text: &str) {
    let (Some(tg), Some(cfg)) = (core.telegram().await, core.config().await) else {
        return;
    };
    if let Err(e) = tg.send_message(cfg.chat_id, text).await {
        log::warn!("could not reply on telegram: {e:#}");
    }
}

async fn allowed(core: &Arc<Core>, user_id: i64) -> bool {
    core.config()
        .await
        .map(|c| c.allowed_user_ids.contains(&user_id))
        .unwrap_or(false)
}

/// One MCP shim's connection, for as long as its client is alive.
async fn serve_client(core: Arc<Core>, stream: TcpStream, token: String) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    // first line has to be a hello with the right token
    let first = lines.next_line().await?.context("client sent nothing")?;
    let (label, cwd) = match serde_json::from_str::<Request>(&first)? {
        Request::Hello { token: t, label, cwd } if t == token => (label, cwd),
        Request::Hello { .. } => anyhow::bail!("bad token"),
        _ => anyhow::bail!("expected hello"),
    };

    let session = core
        .open_session(Kind::Attached, label.clone(), cwd, None, None)
        .await;
    log::info!("session connected: {label} ({session})");

    // the status line this session is editing, if any
    let mut thinking: Option<i64> = None;

    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response { result: format!("bad request: {e}") };
                write.write_all(format!("{}\n", serde_json::to_string(&resp)?).as_bytes()).await?;
                continue;
            }
        };

        let result = match req {
            Request::Hello { .. } => "already connected".to_string(),
            Request::Send { text } => {
                thinking = None;
                core.set_state(&session, State::Working).await;
                core.send(Some(&session), &text).await
            }
            Request::Thinking { text } => {
                core.set_state(&session, State::Working).await;
                let (result, id) = core.thinking(Some(&session), &text, thinking).await;
                thinking = id;
                result
            }
            Request::Ask { question, options } => {
                core.ask(&session, &question, &options).await
            }
        };

        let resp = Response { result };
        write.write_all(format!("{}\n", serde_json::to_string(&resp)?).as_bytes()).await?;
    }

    log::info!("session gone: {label} ({session})");
    core.close_session(&session, None).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::SentMessage;
    use std::path::PathBuf;

    fn core() -> Arc<Core> {
        Core::new(Some(PathBuf::from("/telepager-test/none.json")))
    }

    fn message(text: &str, reply_to: Option<i64>) -> IncomingMessage {
        IncomingMessage {
            from: None,
            text: Some(text.to_string()),
            reply_to_message: reply_to.map(|message_id| SentMessage { message_id }),
        }
    }

    #[test]
    fn callback_data_round_trips() {
        assert_eq!(parse_callback("q3:1"), Some(("q3".into(), 1)));
        assert_eq!(parse_callback("q12:0"), Some(("q12".into(), 0)));
        assert_eq!(parse_callback("nonsense"), None);
        assert_eq!(parse_callback(":2"), None);
        assert_eq!(parse_callback("q1:notanumber"), None);
    }

    #[tokio::test]
    async fn a_slash_message_is_a_command() {
        let c = core();
        assert!(matches!(
            route_message(&c, &message("/status", None), "/status").await,
            Routing::Command(name) if name == "status"
        ));
    }

    #[tokio::test]
    async fn a_plain_message_with_nothing_waiting_goes_to_the_master() {
        let c = core();
        assert!(matches!(
            route_message(&c, &message("what's running?", None), "what's running?").await,
            Routing::Master
        ));
    }

    #[tokio::test]
    async fn help_and_status_work_with_no_config() {
        let c = core();
        // reaching telegram is a no-op unconfigured; this is checking it
        // doesn't panic on the way
        handle_command(&c, "help").await;
        handle_command(&c, "status").await;
        handle_command(&c, "agents").await;
        handle_command(&c, "ui").await;
    }

    #[tokio::test]
    async fn resetting_clears_the_master_conversation() {
        let c = core();
        c.master.lock().await.push(crate::llm::Msg::User("hello".into()));
        assert!(!c.master.lock().await.transcript().is_empty());
        handle_command(&c, "new").await;
        assert!(c.master.lock().await.transcript().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_user_is_ignored() {
        let c = core();
        // no config means no allowlist, so nobody is allowed
        assert!(!allowed(&c, 12345).await);
    }
}
