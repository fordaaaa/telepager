//! The background process everything else talks to: the local socket for MCP
//! shims, the telegram poller and the web console, all over one [`Core`]. Any
//! one of them can be the only one in use — with no token we still come up, so
//! the setup page can hand us one.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::core::{Answer, Core, Fanout};
use crate::ipc::{self, Endpoint, Request, Response};
use crate::master::{self, Origin};
use crate::session::{Kind, State};
use crate::telegram::{IncomingMessage, Telegram};

/// How long a freshly connected client has to introduce itself.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(config: Option<std::path::PathBuf>, open_browser: bool) -> Result<()> {
    // one daemon per machine, or telegram breaks silently: two getUpdates polls
    // on one bot token means each message lands on whichever daemon had the live
    // request, and the newer one overwrites the endpoint file so `master-mcp`
    // resolves to the daemon that didn't get it.
    refuse_if_already_running(crate::client::running_endpoint())?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let core = Core::new(config);
        serve_all(core, open_browser).await
    })
}

/// An endpoint that answers and isn't ours means another daemon already owns
/// this machine's bot token and endpoint file.
fn refuse_if_already_running(existing: Option<Endpoint>) -> Result<()> {
    let Some(ep) = existing else { return Ok(()) };
    // a daemon re-reading its own endpoint is not a second daemon
    if ep.pid == std::process::id() {
        return Ok(());
    }
    anyhow::bail!(
        "telepager is already running (pid {}){}. stop it with `telepager stop` \
         before starting another — two daemons share one bot token and telegram \
         gives each message to only one of them.",
        ep.pid,
        ep.ui_url().map(|u| format!(", console at {u}")).unwrap_or_default()
    )
}

pub async fn serve_all(core: Arc<Core>, open_browser: bool) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding the daemon socket")?;
    let port = listener.local_addr()?.port();
    let token = ipc::new_token();

    // the console comes up first so its url can go in the endpoint file
    let ui = crate::web::start(core.clone(), open_browser, 0).await?;
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
        // one bad accept is not a reason to end the app. a peer that hung up
        // before being accepted, or a moment with no fds, used to take the
        // daemon down through this `?` — poller, console and live sessions
        // with it.
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("could not accept a client: {e}");
                // enough of a pause that a persistent failure doesn't spin
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let core = core.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_client(core, stream, token).await {
                log::debug!("session ended: {e:#}");
            }
        });
    }
}

/// Poll telegram for as long as there's a token, restarting when the config
/// changes under us — that's what makes the setup page take effect live.
async fn telegram_loop(core: Arc<Core>) {
    loop {
        // register for the next change *before* reading the config:
        // notify_waiters wakes only already-registered waiters and stores no
        // permit, so a token saved in the gap would leave us asleep forever.
        let changed = core.config_changed.notified();
        tokio::pin!(changed);

        let Some(tg) = core.telegram().await else {
            // nothing to poll yet; wait for setup to write a token
            changed.await;
            continue;
        };

        tokio::select! {
            _ = poll_updates(core.clone(), tg) => {}
            _ = changed => {
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
            Err(e) if crate::telegram::is_conflict(&e) => {
                log::warn!(
                    "another telepager is polling this bot token — messages will \
                     reach only one of us. stop the other one with `telepager stop`."
                );
                core.notice("another telepager is polling the same bot token").await;
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Err(e) => {
                log::debug!("getUpdates failed, retrying: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.update_id + 1;

            if let Some(msg) = update.message {
                // off the poll loop: a message that blocks — shell command,
                // master turn, anything that asks — must not stop us reading
                // the tap that answers it
                tokio::spawn(handle_message(core.clone(), msg));
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
    Answer(String, Answer),
    /// Hand it to the master agent.
    Master,
    Command(String),
}

async fn route_message(core: &Arc<Core>, msg: &IncomingMessage, text: &str) -> Routing {
    if let Some(rest) = text.strip_prefix('/') {
        return Routing::Command(rest.trim().to_string());
    }

    // replying to the question's own message is the unambiguous case, and the
    // one place free text is taken at face value
    if let Some(replied) = &msg.reply_to_message {
        if let Some(qid) = core.question_for_message(replied.message_id).await {
            return Routing::Answer(qid, Answer::Typed(text.to_string()));
        }
    }

    // a bare message answers the waiting question only when it plainly is an
    // answer. this used to swallow everything, so anything said to the master
    // while an unrelated worker was blocked never reached it.
    if let Some((qid, options)) = core.only_pending_question().await {
        if let Some(answer) = as_answer(text, &options) {
            return Routing::Answer(qid, answer);
        }
    }

    Routing::Master
}

/// A typed message read as an answer: one of the options, or the number on its
/// button. Anything else isn't one.
fn as_answer(text: &str, options: &[String]) -> Option<Answer> {
    let text = text.trim().to_lowercase();
    if let Some(i) = options.iter().position(|o| o.trim().to_lowercase() == text) {
        return Some(Answer::Tapped(i));
    }
    match text.parse::<usize>() {
        Ok(n) if n >= 1 && n <= options.len() => Some(Answer::Tapped(n - 1)),
        _ => None,
    }
}

async fn handle_message(core: Arc<Core>, msg: IncomingMessage) {
    let core = &core;
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
        Routing::Answer(qid, answer) => {
            if !core.answer(&qid, answer).await {
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
        "a" | "answer" => answer_command(core, rest).await,
        "sh" | "run" => master::shell(core, rest, None, None, Origin::Telegram).await,
        "cd" => cd_command(core, rest).await,
        "settings" => settings_command(core).await,
        "kill" => {
            if rest.is_empty() {
                "Say which session, e.g. /kill s3.".into()
            } else {
                match core.kill_agent(rest).await {
                    Ok(()) => format!("Killed {rest}."),
                    Err(e) => format!("Could not: {e:#}"),
                }
            }
        }
        // anything else is just talk aimed at the master agent, argument and all
        _ => match master::reply(core, &as_prompt(name, rest), Origin::Telegram).await {
            Ok(text) => text,
            Err(e) => format!("⚠️ {e:#}"),
        },
    };

    reply_to_telegram(core, &reply).await;
}

/// What an unknown `/command` says to the master. The argument used to be
/// dropped here, so "/deploy the api" arrived as "deploy".
fn as_prompt(name: &str, rest: &str) -> String {
    if rest.is_empty() {
        name.to_string()
    } else {
        format!("{name} {rest}")
    }
}

/// `/a <text>` — the escape hatch for answering the waiting question when the
/// text isn't one of its options.
async fn answer_command(core: &Arc<Core>, rest: &str) -> String {
    if rest.is_empty() {
        return "Say what to answer with, e.g. /a yes.".into();
    }
    let Some((qid, options)) = core.only_pending_question().await else {
        return match core.pending_questions().await.len() {
            0 => "Nothing is waiting on an answer.".into(),
            _ => "More than one question is waiting — tap a button, or reply to the one you mean.".into(),
        };
    };
    let answer = as_answer(rest, &options).unwrap_or_else(|| Answer::Typed(rest.to_string()));
    if core.answer(&qid, answer).await {
        format!("Answered {qid}.")
    } else {
        "That question isn't waiting any more.".into()
    }
}

/// `/cd <dir>` — where later `/sh` runs. Telegram is remote, so it has to be an
/// allowed directory.
async fn cd_command(core: &Arc<Core>, rest: &str) -> String {
    if rest.is_empty() {
        return match core.work_dir().await {
            Some(p) => format!("Working directory is {}.", p.display()),
            None => "Say which directory, e.g. /cd ~/code/api.".into(),
        };
    }

    let Some(cfg) = core.config().await else {
        return "Not set up yet.".into();
    };
    let path = match crate::agents::resolve_dir(rest) {
        Ok(p) => p,
        Err(e) => return format!("Could not use that directory: {e:#}"),
    };

    if !crate::agents::dir_is_allowed(&path, &cfg.allowed_dirs) {
        if cfg.allowed_dirs.is_empty() {
            return format!(
                "no directories are allowed from telegram yet. add them in the \
                 console, or set allowed_dirs in {}.",
                crate::config::first_candidate()
            );
        }
        return format!("{} is not in allowed_dirs.", path.display());
    }

    core.set_work_dir(path.clone()).await;
    format!("Working directory is now {}.", path.display())
}

/// The question `/settings` asks belongs to no session — it's about the app.
const SETTINGS_SESSION: &str = "settings";

/// `/settings` — what's granted, and, when remote control is on, buttons to
/// change it.
async fn settings_command(core: &Arc<Core>) -> String {
    let Some(cfg) = core.config().await else {
        return "Not set up yet.".into();
    };
    let summary = settings_summary(&cfg, core.work_dir().await.as_deref());

    if !cfg.permissions.remote_control {
        return format!(
            "{summary}\n\nchanging these from telegram is off — turn remote control \
             on in the console's Settings."
        );
    }

    // the summary first, then the buttons, so the question has its context
    core.broadcast(&summary).await;
    let options = vec![
        toggle_label("shell access", cfg.permissions.shell),
        toggle_label("confirm destructive", cfg.permissions.confirm_destructive),
        toggle_label("remote control", cfg.permissions.remote_control),
        "leave it".to_string(),
    ];
    let answer = core.ask(SETTINGS_SESSION, "change what?", &options).await;
    apply_setting(core, cfg.permissions, &answer).await
}

fn settings_summary(cfg: &crate::config::Config, work_dir: Option<&std::path::Path>) -> String {
    let dirs = if cfg.allowed_dirs.is_empty() {
        "  none — telegram can't run or start anything".to_string()
    } else {
        cfg.allowed_dirs
            .iter()
            .map(|d| format!("  • {}", d.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "⚙️ settings\n\n\
         shell access: {}\n\
         remote control: {}\n\
         confirm destructive: {}\n\n\
         directories telegram may use:\n{dirs}\n\n\
         working directory: {}\n\
         master agent: {}",
        on_off(cfg.permissions.shell),
        on_off(cfg.permissions.remote_control),
        on_off(cfg.permissions.confirm_destructive),
        work_dir.map(|p| p.display().to_string()).unwrap_or_else(|| "not set — /cd somewhere".into()),
        cfg.master.provider.as_str(),
    )
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn toggle_label(what: &str, on: bool) -> String {
    format!("turn {what} {}", if on { "off" } else { "on" })
}

/// Apply whichever toggle the tap picked, and say what everything is now.
async fn apply_setting(
    core: &Arc<Core>,
    mut permissions: crate::config::Permissions,
    answer: &str,
) -> String {
    let answer = answer.to_lowercase();
    if answer.contains("shell access") {
        permissions.shell = !permissions.shell;
    } else if answer.contains("confirm destructive") {
        permissions.confirm_destructive = !permissions.confirm_destructive;
    } else if answer.contains("remote control") {
        permissions.remote_control = !permissions.remote_control;
    } else {
        return "Left them alone.".into();
    }

    if let Err(e) = crate::config::save_permissions(core.config_path.as_deref(), &permissions) {
        return format!("Could not save that: {e:#}");
    }
    core.reload_config().await;

    // everyone allowed hears it, because reply_to_telegram broadcasts
    format!(
        "⚙️ changed from telegram — shell access {}, remote control {}, confirm destructive {}",
        on_off(permissions.shell),
        on_off(permissions.remote_control),
        on_off(permissions.confirm_destructive),
    )
}

const HELP: &str = "\
telepager — talk to me and I'll run agents on your machine.

Just say what you want, e.g. \"start claude in ~/code/api and make the tests pass\"
or \"what's running?\".

/a <text>  answer the question something is waiting on
/sh <cmd>  run a command in the working directory
/cd <dir>  set that working directory
/kill <s>  stop a running session
/settings  what's granted, and change it if remote control is on
/status    what every session is doing
/agents    which agent CLIs are installed
/new       forget our conversation so far
/ui        the console's address
/help      this";

async fn reply_to_telegram(core: &Arc<Core>, text: &str) {
    core.broadcast(text).await;
}

async fn allowed(core: &Arc<Core>, user_id: i64) -> bool {
    core.config()
        .await
        .map(|c| c.allowed_user_ids.contains(&user_id))
        .unwrap_or(false)
}

/// One MCP shim's connection, for as long as its client lives.
async fn serve_client(core: Arc<Core>, stream: TcpStream, token: String) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    // an unauthenticated peer that says nothing would otherwise pin a task and
    // a file descriptor for as long as the daemon lives
    let first = tokio::time::timeout(HELLO_TIMEOUT, lines.next_line())
        .await
        .context("client sent no hello in time")?
        .context("reading the hello")?
        .context("client sent nothing")?;
    let (label, cwd) = match serde_json::from_str::<Request>(&first)? {
        Request::Hello { token: t, label, cwd } if t == token => (label, cwd),
        Request::Hello { .. } => anyhow::bail!("bad token"),
        _ => anyhow::bail!("expected hello"),
    };

    let session = core
        .open_session(Kind::Attached, label.clone(), cwd, None, None)
        .await;
    log::info!("session connected: {label} ({session})");

    // however this ends — goodbye, killed client, a write that fails mid-reply
    // — the session must be marked finished or prune() never collects it
    let result = serve_requests(&core, &session, &mut lines, &mut write).await;

    log::info!("session gone: {label} ({session})");
    core.close_session(&session, None).await;
    result
}

/// Split out so the caller can close the session on every exit path, errors
/// included.
async fn serve_requests(
    core: &Arc<Core>,
    session: &str,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    write: &mut tokio::net::tcp::OwnedWriteHalf,
) -> Result<()> {
    // the status line this session is editing, one message per chat
    let mut thinking = Fanout::default();

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
                thinking = Fanout::default();
                core.set_state(session, State::Working).await;
                core.send(Some(session), &text).await
            }
            Request::Thinking { text } => {
                core.set_state(session, State::Working).await;
                let (result, sent) = core.thinking(Some(session), &text, &thinking).await;
                thinking = sent;
                result
            }
            Request::Ask { question, options } => {
                core.ask(session, &question, &options).await
            }
        };

        let resp = Response { result };
        write.write_all(format!("{}\n", serde_json::to_string(&resp)?).as_bytes()).await?;
    }

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

    fn endpoint(pid: u32) -> Endpoint {
        Endpoint {
            port: 44013,
            token: "t".into(),
            pid,
            ui_port: Some(46375),
            ui_key: Some("k".into()),
        }
    }

    #[test]
    fn a_second_daemon_refuses_to_start_while_another_one_answers() {
        // this is what split a bot token in two: both daemons polled, and the
        // newer one's endpoint file sent the mcp bridge to the wrong daemon
        let err = refuse_if_already_running(Some(endpoint(4756))).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("already running"), "{text}");
        assert!(text.contains("4756"), "{text}");
        assert!(text.contains("telepager stop"), "{text}");

        // nothing running, or our own endpoint read back, is not a second daemon
        refuse_if_already_running(None).unwrap();
        refuse_if_already_running(Some(endpoint(std::process::id()))).unwrap();
    }

    #[test]
    fn a_poll_conflict_is_told_apart_from_an_ordinary_failure() {
        let conflict = anyhow::anyhow!(
            "telegram rejected getUpdates: Conflict: terminated by other getUpdates \
             request; make sure that only one bot instance is running"
        );
        assert!(crate::telegram::is_conflict(&conflict));
        assert!(!crate::telegram::is_conflict(&anyhow::anyhow!("calling getUpdates: connection reset")));
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

    #[test]
    fn an_option_or_its_number_is_an_answer_and_nothing_else_is() {
        let options = vec!["yes".to_string(), "no".to_string()];

        assert!(matches!(as_answer("yes", &options), Some(Answer::Tapped(0))));
        assert!(matches!(as_answer("  NO ", &options), Some(Answer::Tapped(1))));
        assert!(matches!(as_answer("2", &options), Some(Answer::Tapped(1))));

        // the things that used to be swallowed as answers
        assert!(as_answer("deploy the api", &options).is_none());
        assert!(as_answer("0", &options).is_none());
        assert!(as_answer("3", &options).is_none());
        assert!(as_answer("yes please", &options).is_none());
    }

    // regression: any bare message was taken as the answer to whatever question
    // happened to be waiting, so the master never heard it
    #[tokio::test]
    async fn a_bare_message_only_answers_when_it_really_is_one() {
        let (c, _seen, server) = crate::core::tests::wired_core(&[7]).await;
        let id = c
            .open_session(Kind::Attached, "proj".into(), None, None, None)
            .await;

        let asker = {
            let c = c.clone();
            let id = id.clone();
            tokio::spawn(async move { c.ask(&id, "ship it?", &["yes".into(), "no".into()]).await })
        };
        let qid = crate::core::tests::waiting_question(&c).await;

        assert!(matches!(
            route_message(&c, &message("deploy the api", None), "deploy the api").await,
            Routing::Master
        ));
        // a reply to the question's own message is free text, and still an answer
        assert!(matches!(
            route_message(&c, &message("whatever you think", Some(11)), "whatever you think").await,
            Routing::Answer(id, Answer::Typed(_)) if id == qid
        ));
        assert!(matches!(
            route_message(&c, &message("no", None), "no").await,
            Routing::Answer(id, Answer::Tapped(1)) if id == qid
        ));

        c.answer(&qid, Answer::Tapped(0)).await;
        assert_eq!(asker.await.unwrap(), "yes");
        server.abort();
    }

    #[tokio::test]
    async fn the_answer_command_answers_the_waiting_question() {
        let (c, _seen, server) = crate::core::tests::wired_core(&[7]).await;
        assert!(answer_command(&c, "").await.contains("what to answer"));
        assert!(answer_command(&c, "yes").await.contains("Nothing is waiting"));

        let id = c
            .open_session(Kind::Attached, "proj".into(), None, None, None)
            .await;
        let asker = {
            let c = c.clone();
            let id = id.clone();
            tokio::spawn(async move { c.ask(&id, "which branch?", &["main".into()]).await })
        };
        crate::core::tests::waiting_question(&c).await;

        assert!(answer_command(&c, "the release one").await.starts_with("Answered"));
        assert_eq!(asker.await.unwrap(), "the release one");
        server.abort();
    }

    // regression: "/deploy the api" reached the master as "deploy"
    #[test]
    fn an_unknown_command_keeps_its_argument() {
        assert_eq!(as_prompt("deploy", "the api"), "deploy the api");
        assert_eq!(as_prompt("deploy", ""), "deploy");
    }

    /// Real config file (anything that saves needs one) plus a fake telegram,
    /// so nothing reaches the network.
    async fn configured(name: &str, permissions: crate::config::Permissions, allowed: Vec<PathBuf>)
        -> (Arc<Core>, PathBuf, tokio::task::JoinHandle<()>)
    {
        let dir = std::env::temp_dir().join(format!("telepager-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        crate::config::save(Some(&path), "1:fake", &[7]).unwrap();

        let (base, _seen, server) = crate::core::tests::fake_telegram(7).await;
        let c = Core::new(Some(path.clone()));
        let cfg = crate::config::Config {
            permissions,
            allowed_dirs: allowed,
            ..crate::core::tests::config_for(&[7])
        };
        c.attach(cfg, crate::telegram::Telegram::with_base(&base)).await;
        (c, dir, server)
    }

    #[tokio::test]
    async fn the_new_commands_do_not_panic_with_no_config() {
        let c = core();
        handle_command(&c, "sh echo hi").await;
        handle_command(&c, "cd /tmp").await;
        handle_command(&c, "settings").await;
        handle_command(&c, "kill s9").await;
    }

    #[tokio::test]
    async fn cd_is_held_to_the_allowlist() {
        let root = std::env::temp_dir().join(format!("telepager-cd-{}", std::process::id()));
        let inside = root.join("project");
        std::fs::create_dir_all(&inside).unwrap();

        let (c, dir, server) = configured("cd", Default::default(), vec![]).await;
        assert!(cd_command(&c, inside.to_str().unwrap()).await.contains("no directories are allowed"));
        assert!(c.work_dir().await.is_none());
        server.abort();

        let (c, dir2, server) = configured("cd2", Default::default(), vec![root.clone()]).await;
        assert!(cd_command(&c, inside.to_str().unwrap()).await.contains("now"));
        assert_eq!(c.work_dir().await.unwrap(), inside.canonicalize().unwrap());
        // somewhere real, but not allowed. temp_dir holds root, so it's
        // outside it — and unlike /etc it exists on windows too.
        let outside = std::env::temp_dir();
        assert!(cd_command(&c, outside.to_str().unwrap()).await.contains("not in allowed_dirs"));
        // and a path that isn't a directory at all
        let missing = root.join("nope");
        assert!(cd_command(&c, missing.to_str().unwrap()).await.contains("Could not use"));

        server.abort();
        for d in [root, dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[tokio::test]
    async fn settings_are_read_only_until_remote_control_is_on() {
        let (c, dir, server) = configured("settings-ro", Default::default(), vec![]).await;
        let out = settings_command(&c).await;

        assert!(out.contains("shell access: off"), "{out}");
        assert!(out.contains("remote control: off"), "{out}");
        assert!(out.contains("confirm destructive: on"), "{out}");
        assert!(out.contains("changing these from telegram is off"), "{out}");
        // nothing was asked, so nothing could have been changed
        assert!(c.pending_questions().await.is_empty());

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn remote_control_lets_a_tap_grant_shell_access() {
        let permissions = crate::config::Permissions {
            remote_control: true,
            ..Default::default()
        };
        let (c, dir, server) = configured("settings-rw", permissions, vec![]).await;
        let path = dir.join("config.json");

        let asked = {
            let c = c.clone();
            tokio::spawn(async move { settings_command(&c).await })
        };
        let qid = crate::core::tests::waiting_question(&c).await;
        // the first option is the shell toggle
        c.answer(&qid, Answer::Tapped(0)).await;

        let out = asked.await.unwrap();
        assert!(out.contains("shell access on"), "{out}");
        // and it's in the file, not just in memory
        assert!(crate::config::load(Some(&path)).unwrap().permissions.shell);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn leaving_settings_alone_changes_nothing() {
        let permissions = crate::config::Permissions {
            remote_control: true,
            ..Default::default()
        };
        let (c, dir, server) = configured("settings-leave", permissions, vec![]).await;

        let out = apply_setting(&c, Default::default(), "leave it").await;
        assert_eq!(out, "Left them alone.");
        assert!(!crate::config::load(Some(&dir.join("config.json"))).unwrap().permissions.shell);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_help_lists_the_commands_that_exist() {
        for command in ["/sh", "/cd", "/kill", "/settings", "/a", "/status"] {
            assert!(HELP.contains(command), "{command} isn't in the help");
        }
    }

    #[test]
    fn a_toggle_offers_the_opposite_of_what_is_set() {
        assert_eq!(toggle_label("shell access", false), "turn shell access on");
        assert_eq!(toggle_label("shell access", true), "turn shell access off");
    }

    #[tokio::test]
    async fn an_unknown_user_is_ignored() {
        let c = core();
        // no config means no allowlist, so nobody is allowed
        assert!(!allowed(&c, 12345).await);
    }
}
