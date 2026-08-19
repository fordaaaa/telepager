use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};

use crate::config;
use crate::ipc;
use crate::telegram::Telegram;

const PAGE: &str = include_str!("web_page.html");

// short enough that the browser request doesn't look hung, long enough that
// you can alt-tab to telegram and type /start before it comes back
const DETECT_POLL_SECONDS: u64 = 15;
const MAX_BODY: usize = 64 * 1024;

struct App {
    // a per-run secret in the url, so a stray page in another tab can't
    // read your token off a port it guessed
    key: String,
    config: Option<PathBuf>,
    // getUpdates offset for the "message your bot" step, so polling twice
    // doesn't keep re-reading the update we already answered with
    offset: Mutex<i64>,
    quit: Notify,
}

pub fn run(config: Option<PathBuf>, port: u16, open_browser: bool) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    rt.block_on(serve(config, port, open_browser))
}

async fn serve(config: Option<PathBuf>, port: u16, open_browser: bool) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let addr = listener.local_addr()?;

    let app = Arc::new(App {
        key: ipc::new_token(),
        config,
        offset: Mutex::new(0),
        quit: Notify::new(),
    });

    let url = format!("http://127.0.0.1:{}/?k={}", addr.port(), app.key);
    println!("telepager setup is at:");
    println!();
    println!("  {url}");
    println!();
    println!("leave this running while you use it. ctrl-c when you're done.");

    if open_browser {
        open_in_browser(&url);
    }

    loop {
        tokio::select! {
            _ = app.quit.notified() => {
                println!("done — closing setup.");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting a connection")?;
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(app, stream).await {
                        log::debug!("setup connection ended: {e:#}");
                    }
                });
            }
        }
    }
}

async fn handle(app: Arc<App>, mut stream: TcpStream) -> Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
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

    if req.key.as_deref() != Some(app.key.as_str()) {
        return reply_json(
            &mut stream,
            403,
            &json!({ "error": "wrong setup key — open the link printed in your terminal" }),
        )
        .await;
    }

    let body: Value = if req.body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}))
    };

    let (status, payload) = match route(&app, req.path.as_str(), body).await {
        Ok(v) => (200, v),
        Err(e) => (400, json!({ "error": format!("{e:#}") })),
    };
    reply_json(&mut stream, status, &payload).await
}

async fn route(app: &Arc<App>, path: &str, body: Value) -> Result<Value> {
    log::debug!("setup api {path}");
    match path {
        "/api/state" => state(app).await,
        "/api/check" => check_token(str_field(&body, "token")?).await,
        "/api/detect" => detect_user(app, str_field(&body, "token")?).await,
        "/api/save" => save(app, &body).await,
        "/api/test" => send_test(app).await,
        "/api/quit" => {
            app.quit.notify_one();
            Ok(json!({ "ok": true }))
        }
        other => anyhow::bail!("no such endpoint {other}"),
    }
}

async fn state(app: &Arc<App>) -> Result<Value> {
    let path = config::target_path(app.config.as_deref());
    let exists = config::existing_path(app.config.as_deref()).is_some();

    let (configured, ids, problem) = match config::load(app.config.as_deref()) {
        Ok(cfg) => (true, cfg.allowed_user_ids, None),
        Err(e) if exists => (false, vec![], Some(format!("{e:#}"))),
        Err(_) => (false, vec![], None),
    };

    Ok(json!({
        "configured": configured,
        "config_path": path.display().to_string(),
        "allowed_user_ids": ids,
        "problem": problem,
        "daemon_running": daemon_running(),
        "register_command": "claude mcp add --scope user telepager -- telepager mcp",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn check_token(token: &str) -> Result<Value> {
    let me = Telegram::new(token)?.get_me().await?;
    Ok(json!({ "bot": me.display_name(), "bot_id": me.id }))
}

async fn detect_user(app: &Arc<App>, token: &str) -> Result<Value> {
    // the daemon holds the only getUpdates slot telegram gives a bot, so
    // polling behind its back would just trade 409s with it
    if daemon_running() {
        anyhow::bail!(
            "the telepager daemon is running and already listening for updates — \
             stop it first, or type your id in by hand"
        );
    }

    let tg = Telegram::new(token)?;
    let offset = *app.offset.lock().await;
    let updates = tg.get_updates_for(offset, DETECT_POLL_SECONDS).await?;

    let mut found = None;
    let mut next = offset;
    for update in &updates {
        next = update.update_id + 1;
        let from = update
            .message
            .as_ref()
            .and_then(|m| m.from.as_ref())
            .or_else(|| update.callback_query.as_ref().map(|c| &c.from));
        if let Some(user) = from {
            found = Some(json!({ "id": user.id, "name": user.display_name() }));
            break;
        }
    }
    *app.offset.lock().await = next;

    Ok(match found {
        Some(user) => json!({ "found": true, "user": user }),
        None => json!({ "found": false }),
    })
}

async fn save(app: &Arc<App>, body: &Value) -> Result<Value> {
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

    let path = config::save(app.config.as_deref(), token, &ids)?;
    Ok(json!({
        "saved": true,
        "bot": me.display_name(),
        "config_path": path.display().to_string(),
    }))
}

async fn send_test(app: &Arc<App>) -> Result<Value> {
    let cfg = config::load(app.config.as_deref())?;
    Telegram::new(&cfg.token)?
        .send_message(cfg.chat_id, "telepager is set up. this is what a page looks like.")
        .await?;
    Ok(json!({ "sent": true }))
}

fn str_field<'a>(body: &'a Value, name: &str) -> Result<&'a str> {
    body.get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing '{name}'"))
}

fn daemon_running() -> bool {
    use std::net::{Shutdown, TcpStream as StdStream};
    use std::time::Duration;

    let Some(ep) = ipc::read_endpoint() else {
        return false;
    };
    let Ok(addr) = format!("127.0.0.1:{}", ep.port).parse() else {
        return false;
    };
    match StdStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(sock) => {
            let _ = sock.shutdown(Shutdown::Both);
            true
        }
        Err(_) => false,
    }
}

struct Request {
    method: String,
    path: String,
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

    Ok(Some(Request { method, path, host, key, body }))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn query_param(target: &str, name: &str) -> Option<String> {
    let (_, query) = target.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
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

fn open_in_browser(url: &str) {
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
    fn head_end_is_the_blank_line() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn missing_fields_are_an_error_not_a_panic() {
        assert!(str_field(&json!({}), "token").is_err());
        assert_eq!(str_field(&json!({"token": "x"}), "token").unwrap(), "x");
    }
}
