//! The local web console, and the setup page that comes before it. One loopback
//! server does both: no token yet and you get the wizard, and the moment it's
//! saved the same page becomes the console. That's what makes `telepager` one
//! command — you configure the app from inside it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Semaphore;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::agents;
use crate::config::{self, MasterConfig, Provider};
use crate::core::{Answer, Core, UiInfo};
use crate::ipc;
use crate::master::{self, Origin};
use crate::telegram::Telegram;

const PAGE: &str = include_str!("web_page.html");

/// Long enough to alt-tab to Telegram and press start, short enough that the
/// browser request doesn't look hung.
const DETECT_POLL_SECONDS: u64 = 15;
const MAX_BODY: usize = 256 * 1024;
/// How long a connection gets to send request line, headers and body. Read
/// only — an event stream is meant to stay open afterwards.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Ceiling on connections at once, so idle sockets can't exhaust fds. Generous
/// — a console tab uses one stream plus short calls.
const MAX_CONNECTIONS: usize = 64;
/// How much history a freshly opened page is given.
const SNAPSHOT_EVENTS: usize = 400;

/// Bind the console and start serving, returning where it landed.
/// `port_override` is `--port`; 0 means the configured port, or any free one.
pub async fn start(core: Arc<Core>, open_browser: bool, port_override: u16) -> Result<UiInfo> {
    let port = match port_override {
        0 => core
            .config()
            .await
            .map(|c| c.ui_port)
            .unwrap_or(crate::config::DEFAULT_UI_PORT),
        explicit => explicit,
    };

    // a configured port that's taken shouldn't stop the app coming up
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) if port != 0 => {
            log::warn!("could not bind port {port} ({e}), taking a free one instead");
            TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("binding the console socket")?
        }
        Err(e) => return Err(e).context("binding the console socket"),
    };

    let info = UiInfo {
        port: listener.local_addr()?.port(),
        // a per-run secret in the url, so a stray page on another origin can't
        // drive the console on a port it guessed
        key: ipc::new_token(),
    };

    let url = info.url();
    if open_browser {
        open_in_browser(&url);
    }

    let serving = core.clone();
    let key = info.key.clone();
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let core = serving.clone();
                    let key = key.clone();
                    // dropped when the connection ends, releasing the slot
                    let Ok(permit) = permits.clone().acquire_owned().await else {
                        return;
                    };
                    tokio::spawn(async move {
                        if let Err(e) = handle(core, stream, key).await {
                            log::debug!("console connection ended: {e:#}");
                        }
                        drop(permit);
                    });
                }
                // one bad accept isn't the end of the console. returning here
                // left the app running with a port nothing answered on, which
                // looks to the user like telepager quit
                Err(e) => {
                    log::warn!("console accept failed: {e:#}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });

    Ok(info)
}

/// Run the console in the foreground, for `telepager setup` where no daemon
/// should be left behind.
pub fn run_standalone(config: Option<std::path::PathBuf>, port: u16, open: bool) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;

    rt.block_on(async move {
        let core = Core::new(config);
        let info = start(core.clone(), open, port).await?;
        core.set_ui(info.clone()).await;

        println!("telepager console:");
        println!();
        println!("  {}", info.url());
        println!();
        println!("leave this running while you use it. ctrl-c when you're done.");

        std::future::pending::<()>().await;
        Ok(())
    })
}

async fn handle(core: Arc<Core>, mut stream: TcpStream, key: String) -> Result<()> {
    // a peer that connects and then says nothing would otherwise hold a task,
    // a socket and up to MAX_BODY of buffer for as long as the app runs
    let Some(req) = tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream))
        .await
        .context("the request was too slow to arrive")??
    else {
        return Ok(());
    };

    // a page on another origin can reach a localhost port by name, so only
    // answer to the host we actually told the user to visit
    if !host_is_local(&req.host) {
        return reply(&mut stream, 403, "text/plain", b"not for you").await;
    }

    if req.method == "GET" && req.path == "/" {
        return reply(&mut stream, 200, "text/html; charset=utf-8", PAGE.as_bytes()).await;
    }

    if !req.path.starts_with("/api/") {
        return reply(&mut stream, 404, "text/plain", b"no such page").await;
    }

    if req.key.as_deref() != Some(key.as_str()) {
        return reply_json(
            &mut stream,
            403,
            &json!({ "error": "wrong console key — open the link printed in your terminal" }),
        )
        .await;
    }

    // the event stream holds the socket open, so it can't go through reply()
    if req.path == "/api/events" {
        return stream_events(core, stream, &req).await;
    }

    let body: Value = if req.body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(value) => value,
            // saying "missing 'token'" for a syntax error sends people hunting
            // for the wrong problem
            Err(e) => {
                return reply_json(
                    &mut stream,
                    400,
                    &json!({ "error": format!("the request body isn't valid JSON: {e}") }),
                )
                .await
            }
        }
    };

    let (status, payload) = match route(&core, req.path.as_str(), body).await {
        Ok(v) => (200, v),
        Err(e) => (400, json!({ "error": format!("{e:#}") })),
    };
    reply_json(&mut stream, status, &payload).await
}

async fn route(core: &Arc<Core>, path: &str, body: Value) -> Result<Value> {
    log::debug!("console api {path}");
    match path {
        "/api/state" => state(core).await,

        // ---- setup
        "/api/check" => check_token(str_field(&body, "token")?).await,
        "/api/detect" => detect_user(core, str_field(&body, "token")?).await,
        "/api/save" => save(core, &body).await,
        "/api/test" => send_test(core).await,

        // ---- the master agent
        "/api/master/config" => save_master(core, &body).await,
        "/api/master/send" => master_send(core, &body).await,
        "/api/master/reset" => {
            core.master.lock().await.clear();
            Ok(json!({ "ok": true }))
        }
        "/api/master/transcript" => Ok(json!({
            "transcript": core.master.lock().await.transcript(),
        })),

        // ---- sessions and agents
        "/api/spawn" => spawn(core, &body).await,
        "/api/session/kill" => {
            core.kill_agent(str_field(&body, "session")?).await?;
            Ok(json!({ "ok": true }))
        }
        "/api/session/input" => {
            core.write_to_agent(str_field(&body, "session")?, str_field(&body, "text")?).await?;
            Ok(json!({ "ok": true }))
        }
        "/api/session/eof" => {
            core.close_agent_stdin(str_field(&body, "session")?).await?;
            Ok(json!({ "ok": true }))
        }
        "/api/session/screen" => {
            let since = body.get("since").and_then(|v| v.as_u64());
            core.screen_view(str_field(&body, "session")?, since).await
        }
        "/api/session/resize" => {
            let cols = size_field(&body, "cols")?;
            let rows = size_field(&body, "rows")?;
            core.resize_session(str_field(&body, "session")?, cols, rows).await
        }
        "/api/session/forget" => Ok(json!({
            "ok": core.forget_session(str_field(&body, "session")?).await,
        })),
        "/api/answer" => {
            let qid = str_field(&body, "question_id")?;
            let answer = str_field(&body, "answer")?;
            let delivered = core.answer(qid, Answer::Typed(answer.to_string())).await;
            Ok(json!({ "ok": delivered }))
        }
        "/api/dirs" => save_dirs(core, &body).await,
        "/api/permissions" => save_permissions(core, &body).await,

        // the bridge a cli-backed master agent calls its tools through
        "/api/tool" => run_tool(core, &body).await,

        other => anyhow::bail!("no such endpoint {other}"),
    }
}

async fn state(core: &Arc<Core>) -> Result<Value> {
    let path = config::target_path(core.config_path.as_deref());
    let exists = config::existing_path(core.config_path.as_deref()).is_some();

    let (configured, ids, problem) = match config::load(core.config_path.as_deref()) {
        Ok(cfg) => (true, cfg.allowed_user_ids, None),
        Err(e) if exists => (false, vec![], Some(format!("{e:#}"))),
        Err(_) => (false, vec![], None),
    };

    let cfg = core.config().await;
    let master = cfg.as_ref().map(|c| &c.master).cloned().unwrap_or_default();
    let agent_list = cfg
        .as_ref()
        .map(|c| agents::catalog(&c.agents))
        .unwrap_or_else(|| agents::catalog(&config::builtin_agents()));

    Ok(json!({
        "configured": configured,
        "config_path": path.display().to_string(),
        "allowed_user_ids": ids,
        "problem": problem,
        "version": env!("CARGO_PKG_VERSION"),
        "register_command": "claude mcp add --scope user telepager -- telepager mcp",
        "sessions": sessions_with_liveness(core).await,
        "live": core.live_count().await,
        "questions": core.pending_questions().await,
        "agents": agent_list,
        "allowed_dirs": cfg
            .as_ref()
            .map(|c| c.allowed_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>())
            .unwrap_or_default(),
        "permissions": cfg.as_ref().map(|c| c.permissions.clone()).unwrap_or_default(),
        "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
        "home": dirs::home_dir().map(|p| p.display().to_string()),
        "master": {
            "provider": master.provider,
            "model": master.model_or_default(),
            "base_url": master.base_url.clone(),
            "system": master.system.clone(),
            "bundled_mcp": master.bundled_mcp,
            "mcp_servers": master.mcp_servers.clone(),
            "tmux_attach": master.tmux_attach,
            "ready": master.is_usable(),
            "key_source": master_key_source(&master),
            "providers": Provider::all().iter().map(|p| json!({
                "id": p.as_str(),
                "default_model": p.default_model(),
                "key_env": p.default_key_env(),
                "needs_key": p.needs_key(),
                "is_cli": p.is_cli(),
                "blurb": p.blurb(),
                "installed": p.cli_command().map(|c| agents::which(c).is_some()),
                "has_key_in_env": std::env::var(p.default_key_env()).map(|v| !v.trim().is_empty()).unwrap_or(false)
                    || p.fallback_key_envs().iter().any(|n| std::env::var(n).map(|v| !v.trim().is_empty()).unwrap_or(false)),
            })).collect::<Vec<_>>(),
        },
        "transcript": core.master.lock().await.transcript(),
    }))
}

/// The session list, each marked with whether we still hold a live process for
/// it — that's what decides if "kill" and stdin are offered.
async fn sessions_with_liveness(core: &Arc<Core>) -> Vec<Value> {
    crate::tmux::sync(core).await;
    let mut out = Vec::new();
    for mut summary in core.sessions().await {
        let running = match summary["id"].as_str() {
            Some(id) => core.is_running(id).await,
            None => false,
        };
        if let Some(obj) = summary.as_object_mut() {
            obj.insert("running".into(), json!(running));
        }
        out.push(summary);
    }
    out
}

/// Where the master's key comes from, so the UI can say so without showing the
/// key itself.
fn master_key_source(master: &MasterConfig) -> Option<String> {
    if master.provider.is_cli() {
        return Some(format!(
            "the {} cli's own login",
            master.cli_command().unwrap_or_default()
        ));
    }
    if master.api_key.as_deref().map(str::trim).is_some_and(|k| !k.is_empty()) {
        return Some("the config file".into());
    }
    let mut names = Vec::new();
    if let Some(n) = master.api_key_env.as_deref() {
        names.push(n.to_string());
    }
    names.push(master.provider.default_key_env().to_string());
    names.extend(master.provider.fallback_key_envs().iter().map(|s| s.to_string()));

    names.into_iter().find(|name| {
        std::env::var(name).map(|v| !v.trim().is_empty()).unwrap_or(false)
    })
}

async fn check_token(token: &str) -> Result<Value> {
    let me = Telegram::new(token)?.get_me().await?;
    Ok(json!({ "bot": me.display_name(), "bot_id": me.id }))
}

async fn detect_user(core: &Arc<Core>, token: &str) -> Result<Value> {
    // telegram gives a bot one getUpdates slot, and the poller owns it once
    // there's a token. polling behind its back would just trade 409s.
    if core.is_configured().await {
        anyhow::bail!(
            "telepager is already set up and listening for updates. \
             type the id in by hand to change it."
        );
    }

    let tg = Telegram::new(token)?;
    let updates = tg.get_updates_for(0, DETECT_POLL_SECONDS).await?;

    for update in &updates {
        let from = update
            .message
            .as_ref()
            .and_then(|m| m.from.as_ref())
            .or_else(|| update.callback_query.as_ref().map(|c| &c.from));
        if let Some(user) = from {
            return Ok(json!({
                "found": true,
                "user": { "id": user.id, "name": user.display_name() },
            }));
        }
    }
    Ok(json!({ "found": false }))
}

async fn save(core: &Arc<Core>, body: &Value) -> Result<Value> {
    let token = str_field(body, "token")?.trim();
    if token.is_empty() {
        anyhow::bail!("a bot token is required");
    }

    let ids: Vec<i64> = body
        .get("user_ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        anyhow::bail!("at least one user id is required — that allowlist is the whole security model");
    }

    // don't write a token telegram won't accept
    let me = Telegram::new(token)?.get_me().await?;
    let path = config::save(core.config_path.as_deref(), token, &ids)?;

    // picked up without restarting anything
    core.reload_config().await;

    Ok(json!({
        "saved": true,
        "bot": me.display_name(),
        "config_path": path.display().to_string(),
    }))
}

async fn send_test(core: &Arc<Core>) -> Result<Value> {
    // goes to everyone, so the test proves what it looks like it proves
    let (result, fanout) = core
        .broadcast("telepager is set up. this is what a page looks like.")
        .await;
    if fanout.is_empty() {
        anyhow::bail!("{result}");
    }
    Ok(json!({ "sent": true, "chats": fanout.len() }))
}

async fn save_master(core: &Arc<Core>, body: &Value) -> Result<Value> {
    let existing = core.config().await.map(|c| c.master).unwrap_or_default();

    let provider: Provider = match body.get("provider") {
        Some(v) => serde_json::from_value(v.clone()).context("unknown provider")?,
        None => existing.provider,
    };

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let optional = |name: &str| -> Option<String> {
        body.get(name)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let master = MasterConfig {
        provider,
        model,
        // an empty key field means "leave what's there", not "erase it"
        api_key: optional("api_key").or(existing.api_key),
        api_key_env: optional("api_key_env").or(existing.api_key_env),
        base_url: optional("base_url"),
        system: optional("system"),
        max_tokens: body
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(existing.max_tokens),
        command: optional("command").or(existing.command),
        args: existing.args,
        bundled_mcp: body
            .get("bundled_mcp")
            .and_then(|v| v.as_bool())
            .unwrap_or(existing.bundled_mcp),
        mcp_servers: body
            .get("mcp_servers")
            .and_then(|v| v.as_object())
            .cloned()
            .or(existing.mcp_servers),
        tmux_attach: body
            .get("tmux_attach")
            .and_then(|v| v.as_bool())
            .unwrap_or(existing.tmux_attach),
    };

    let switched = master.provider != existing.provider;
    config::save_master(core.config_path.as_deref(), &master)?;
    core.reload_config().await;

    if switched {
        // a cli session id means nothing to another backend, and tool results
        // left behind by one would confuse the next
        core.master.lock().await.clear();
    }

    let ready = master.is_usable();
    let problem = (!ready).then(|| {
        if provider.is_cli() {
            format!(
                "the {} cli isn't installed, or isn't on telepager's PATH",
                master.cli_command().unwrap_or_default()
            )
        } else {
            format!(
                "no api key yet — set {} in the environment telepager runs in, or paste one above",
                provider.default_key_env()
            )
        }
    });

    Ok(json!({
        "ok": true,
        "ready": ready,
        "switched": switched,
        "model": if provider.is_cli() && master.model.trim().is_empty() {
            format!("{}'s default model", provider.as_str())
        } else {
            master.model_or_default()
        },
        "problem": problem,
    }))
}

async fn master_send(core: &Arc<Core>, body: &Value) -> Result<Value> {
    let text = str_field(body, "text")?.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("nothing to send");
    }
    // the console is local, so it isn't held to the telegram directory allowlist
    let answer = master::reply(core, &text, Origin::Ui).await?;
    // said at the console, but everyone should still hear it
    core.announce(answer.clone());
    Ok(json!({ "reply": answer }))
}

async fn spawn(core: &Arc<Core>, body: &Value) -> Result<Value> {
    let agent = str_field(body, "agent")?;
    let dir = str_field(body, "dir")?;
    let task = str_field(body, "task")?;
    // optional: most agents take whatever model they're already set to
    let model = body.get("model").and_then(|v| v.as_str()).map(str::trim).filter(|m| !m.is_empty());
    let id = core.spawn_agent(agent, dir, task, model, false).await?;
    Ok(json!({ "session": id }))
}

/// Run one master-agent tool and hand back its text. `telepager master-mcp`
/// exposes these over MCP and proxies each call here, so a cli-backed master
/// shares the one implementation.
async fn run_tool(core: &Arc<Core>, body: &Value) -> Result<Value> {
    let name = str_field(body, "name")?.to_string();
    let args = body.get("args").cloned().unwrap_or_else(|| json!({}));
    // a caller that doesn't say gets the gated treatment, not the trusted one
    let origin = Origin::parse(body.get("origin").and_then(|v| v.as_str()).unwrap_or_default());

    let call = crate::llm::ToolCall { id: String::new(), name: name.clone(), args };
    let result = master::run_tool(core, &call, origin).await;

    core.master.lock().await.push(crate::llm::Msg::ToolResult {
        id: String::new(),
        name: name.clone(),
        content: result.clone(),
    });

    // so the console shows what a cli master is doing, the same way it shows
    // the http one's tool calls
    core.record(
        None,
        crate::session::EventKind::Chat {
            role: "tool".into(),
            text: format!("{name} → {}", result.lines().next().unwrap_or_default()),
        },
    )
    .await;

    Ok(json!({ "result": result }))
}

async fn save_dirs(core: &Arc<Core>, body: &Value) -> Result<Value> {
    let dirs: Vec<std::path::PathBuf> = body
        .get("dirs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .collect()
        })
        .unwrap_or_default();

    config::save_allowed_dirs(core.config_path.as_deref(), &dirs)?;
    core.reload_config().await;
    Ok(json!({ "ok": true, "count": dirs.len() }))
}

/// Grant or revoke what telepager may do. A missing field keeps its value, so
/// the console can save one checkbox without knowing the rest.
async fn save_permissions(core: &Arc<Core>, body: &Value) -> Result<Value> {
    let existing = core.config().await.map(|c| c.permissions).unwrap_or_default();
    let flag = |name: &str, current: bool| body.get(name).and_then(|v| v.as_bool()).unwrap_or(current);

    let permissions = config::Permissions {
        shell: flag("shell", existing.shell),
        remote_control: flag("remote_control", existing.remote_control),
        confirm_destructive: flag("confirm_destructive", existing.confirm_destructive),
    };

    config::save_permissions(core.config_path.as_deref(), &permissions)?;
    core.reload_config().await;

    // a grant is worth hearing about wherever you are, not just where you
    // ticked the box
    if permissions.shell != existing.shell || permissions.remote_control != existing.remote_control {
        core.announce(format!(
            "⚙️ changed in the console — shell access {}, remote control {}",
            if permissions.shell { "on" } else { "off" },
            if permissions.remote_control { "on" } else { "off" },
        ));
    }

    Ok(json!({ "ok": true, "permissions": permissions }))
}

/// Server-sent events: the snapshot the page draws itself from, then everything
/// that happens after.
async fn stream_events(core: Arc<Core>, mut stream: TcpStream, req: &Request) -> Result<()> {
    let session = query_param(&req.target, "session");
    let mut rx = core.subscribe().await;

    let head = "HTTP/1.1 200 OK\r\n\
                content-type: text/event-stream\r\n\
                cache-control: no-store\r\n\
                connection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;

    for event in core.snapshot(session.as_deref(), SNAPSHOT_EVENTS).await {
        write_event(&mut stream, &event).await?;
    }
    // tells the page the backlog is done and it can stop showing a spinner
    stream.write_all(b"event: ready\ndata: {}\n\n").await?;
    stream.flush().await?;

    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Ok(event) => {
                    if let Some(want) = &session {
                        // app-level events have no session and are always sent
                        if event.session.as_deref().is_some_and(|s| s != want) {
                            continue;
                        }
                    }
                    write_event(&mut stream, &event).await?;
                    stream.flush().await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::debug!("a console fell {n} events behind");
                }
                Err(_) => return Ok(()),
            },
            // a comment keeps the connection alive through proxies and lets a
            // write fail promptly once the tab is closed
            _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {
                stream.write_all(b": keepalive\n\n").await?;
                stream.flush().await?;
            }
        }
    }
}

async fn write_event(stream: &mut TcpStream, event: &crate::session::Event) -> Result<()> {
    let payload = serde_json::to_string(event)?;
    stream
        .write_all(format!("id: {}\ndata: {payload}\n\n", event.seq).as_bytes())
        .await?;
    Ok(())
}

fn str_field<'a>(body: &'a Value, name: &str) -> Result<&'a str> {
    body.get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing '{name}'"))
}

/// A terminal dimension: a positive whole number the screen can clamp.
fn size_field(body: &Value, name: &str) -> Result<u16> {
    let value = body
        .get(name)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing '{name}'"))?;
    if value == 0 {
        anyhow::bail!("'{name}' has to be at least 1");
    }
    Ok(value.min(u16::MAX as u64) as u16)
}

struct Request {
    method: String,
    path: String,
    /// The path with its query string still attached.
    target: String,
    host: String,
    key: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];

    let head_end = loop {
        if let Some(i) = find_head_end(&buf) {
            break i;
        }
        if buf.len() > MAX_BODY {
            anyhow::bail!("request headers are absurdly long");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let start = lines.next().unwrap_or_default();
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut host = String::new();
    let mut content_length = 0usize;
    let mut key_header = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => host = value.to_string(),
            "content-length" => content_length = value.parse().unwrap_or(0),
            "x-telepager-key" => key_header = Some(value.to_string()),
            _ => {}
        }
    }

    if content_length > MAX_BODY {
        anyhow::bail!("request body is too big");
    }

    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    // the key rides in the url for the page load and in a header after that
    let key = key_header.or_else(|| query_param(&target, "k"));
    let path = target.split('?').next().unwrap_or("/").to_string();

    Ok(Some(Request { method, path, target, host, key, body }))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn query_param(target: &str, name: &str) -> Option<String> {
    let (_, query) = target.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

/// Enough percent decoding for a session id or a path in a query string. Works
/// on bytes: slicing the &str by byte offsets panics when an escape lands
/// mid-character, on input that reaches us before any auth.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => match hex_pair(bytes, i) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                // a malformed escape is a literal percent sign
                None => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The two hex digits after a `%`, if they're both there and both hex.
fn hex_pair(bytes: &[u8], at: usize) -> Option<u8> {
    let hi = hex_digit(*bytes.get(at + 1)?)?;
    let lo = hex_digit(*bytes.get(at + 2)?)?;
    Some(hi * 16 + lo)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn host_is_local(host: &str) -> bool {
    let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

async fn reply_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<()> {
    let text = serde_json::to_vec(body)?;
    reply(stream, status, "application/json", &text).await
}

async fn reply(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: {content_type}\r\n\
         content-length: {}\r\n\
         cache-control: no-store\r\n\
         connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

pub fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };

    // no browser is a normal thing on a server — the url is on stdout anyway
    let _ = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn core() -> Arc<Core> {
        Core::new(Some(PathBuf::from("/telepager-test/none.json")))
    }

    #[test]
    fn only_localhost_hosts_are_served() {
        assert!(host_is_local("127.0.0.1:8765"));
        assert!(host_is_local("localhost:8765"));
        assert!(host_is_local("localhost"));
        assert!(!host_is_local("evil.example.com:8765"));
        assert!(!host_is_local("127.0.0.1.evil.com"));
    }

    #[test]
    fn key_comes_out_of_the_query() {
        assert_eq!(query_param("/?k=abc", "k"), Some("abc".into()));
        assert_eq!(query_param("/api/state?x=1&k=zz", "k"), Some("zz".into()));
        assert_eq!(query_param("/", "k"), None);
        assert_eq!(query_param("/?j=1", "k"), None);
    }

    #[test]
    fn query_values_are_percent_decoded() {
        assert_eq!(query_param("/api/events?session=s%201", "session"), Some("s 1".into()));
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%2Ftmp%2Fx"), "/tmp/x");
        // a broken escape is left alone rather than dropped
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn a_malformed_escape_never_panics() {
        // regression: these used to slice the &str mid-character and panic,
        // on input that arrives before any authentication
        assert_eq!(percent_decode("%\u{20AC}"), "%\u{20AC}");
        assert_eq!(percent_decode("%é"), "%é");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%2"), "%2");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("100%"), "100%");
        // and the valid cases still decode
        assert_eq!(percent_decode("%2Ftmp%2Fx"), "/tmp/x");
        assert_eq!(percent_decode("%7e"), "~");
        assert_eq!(percent_decode("a+b"), "a b");
    }

    #[test]
    fn hex_pairs_are_only_accepted_when_both_digits_are_hex() {
        assert_eq!(hex_pair(b"%41", 0), Some(0x41));
        assert_eq!(hex_pair(b"%4g", 0), None);
        assert_eq!(hex_pair(b"%4", 0), None);
    }

    #[test]
    fn head_end_is_the_blank_line() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn missing_fields_are_an_error_not_a_panic() {
        assert!(str_field(&json!({}), "token").is_err());
        assert_eq!(str_field(&json!({"token": "x"}), "token").unwrap(), "x");
    }

    #[tokio::test]
    async fn state_works_before_anything_is_configured() {
        let s = state(&core()).await.unwrap();
        assert_eq!(s["configured"], false);
        // the agent picker is still populated, so the console isn't empty
        assert!(s["agents"].as_array().unwrap().len() > 3);
        let providers = s["master"]["providers"].as_array().unwrap();
        assert_eq!(providers.len(), Provider::all().len());
        // the cli backends are offered first, since they need no api key
        assert_eq!(providers[0]["id"], "claude-code");
        assert_eq!(providers[0]["is_cli"], true);
        assert_eq!(providers[0]["needs_key"], false);
        assert!(s["sessions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_terminal_size_has_to_be_a_positive_number() {
        let body = json!({ "cols": 100, "rows": 0, "wrong": "80" });
        assert_eq!(size_field(&body, "cols").unwrap(), 100);
        assert!(size_field(&body, "rows").is_err());
        assert!(size_field(&body, "wrong").is_err());
        assert!(size_field(&body, "missing").is_err());
        // more columns than a u16 holds is clamped, not wrapped
        assert_eq!(size_field(&json!({ "cols": 999999 }), "cols").unwrap(), u16::MAX);
    }

    #[tokio::test]
    async fn unknown_endpoints_are_rejected() {
        assert!(route(&core(), "/api/nope", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn saving_without_a_token_or_ids_is_refused() {
        let c = core();
        assert!(save(&c, &json!({ "token": "" })).await.is_err());
        assert!(save(&c, &json!({ "token": "x", "user_ids": [] })).await.is_err());
    }

    #[test]
    fn the_key_source_never_reveals_the_key() {
        let master = MasterConfig {
            provider: Provider::Anthropic,
            api_key: Some("sk-secret".into()),
            ..MasterConfig::default()
        };
        let source = master_key_source(&master).unwrap();
        assert_eq!(source, "the config file");
        assert!(!source.contains("sk-secret"));

        // a cli backend has no key to reveal in the first place
        let cli = MasterConfig::default();
        assert_eq!(master_key_source(&cli).unwrap(), "the claude cli's own login");
    }

    #[tokio::test]
    async fn an_explicit_port_is_honoured() {
        // 0 would mean "pick anything", which wouldn't prove the override
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free = listener.local_addr().unwrap().port();
        drop(listener);

        let info = start(core(), false, free).await.unwrap();
        assert_eq!(info.port, free);
    }

    #[tokio::test]
    async fn a_taken_port_falls_back_instead_of_failing() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap().port();

        let info = start(core(), false, taken).await.unwrap();
        assert_ne!(info.port, taken, "should have moved off the busy port");
        assert_ne!(info.port, 0);
    }

    #[tokio::test]
    async fn a_tool_call_lands_in_the_transcript() {
        let c = core();
        run_tool(&c, &json!({ "name": "list_sessions", "args": {} })).await.unwrap();

        let transcript = c.master.lock().await.transcript();
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0]["role"], "tool");
        assert_eq!(transcript[0]["name"], "list_sessions");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_reported_rather_than_failing_the_request() {
        let c = core();
        let out = run_tool(&c, &json!({ "name": "nonsense", "args": {} })).await.unwrap();
        assert!(out["result"].as_str().unwrap().contains("no tool called"));
    }

    #[tokio::test]
    async fn a_fresh_install_is_granted_nothing() {
        let s = state(&core()).await.unwrap();
        assert_eq!(s["permissions"]["shell"], false);
        assert_eq!(s["permissions"]["remote_control"], false);
        assert_eq!(s["permissions"]["confirm_destructive"], true);
    }

    #[tokio::test]
    async fn saving_one_permission_leaves_the_others_where_they_were() {
        let dir = std::env::temp_dir().join(format!("telepager-web-perms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        config::save(Some(&path), "1:fake", &[7]).unwrap();
        let c = Core::new(Some(path.clone()));

        let out = save_permissions(&c, &json!({ "shell": true })).await.unwrap();
        assert_eq!(out["permissions"]["shell"], true);
        // the ones the console didn't send keep their values
        assert_eq!(out["permissions"]["confirm_destructive"], true);
        assert_eq!(out["permissions"]["remote_control"], false);

        let saved = config::load(Some(&path)).unwrap().permissions;
        assert!(saved.shell);
        assert!(saved.confirm_destructive);

        // and the token is still there
        assert_eq!(config::load(Some(&path)).unwrap().token, "1:fake");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_blank_master_message_is_refused() {
        assert!(master_send(&core(), &json!({ "text": "   " })).await.is_err());
    }
}
