use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};

use crate::config::Config;
use crate::ipc::{self, Endpoint, Request, Response};
use crate::telegram::Telegram;

const MAX_OPTIONS: usize = 20;

// how a question got answered
#[derive(Debug)]
pub enum Answer {
    Tapped(usize),
    Typed(String),
}

// a question waiting on an answer, keyed by the message the buttons are on
type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Answer>>>>;

struct Daemon {
    cfg: Config,
    tg: Telegram,
    pending: Pending,
}

pub fn run(config: Option<std::path::PathBuf>) -> Result<()> {
    let cfg = crate::config::load(config.as_deref())?;
    let tg = Telegram::new(&cfg.token)?;
    let daemon = Arc::new(Daemon {
        cfg,
        tg,
        pending: Arc::new(Mutex::new(HashMap::new())),
    });

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding the daemon socket")?;
        let port = listener.local_addr()?.port();
        let token = ipc::new_token();

        ipc::write_endpoint(&Endpoint {
            port,
            token: token.clone(),
            pid: std::process::id(),
        })?;
        log::info!("daemon listening on 127.0.0.1:{port}");

        tokio::spawn(poll_updates(daemon.clone()));

        loop {
            let (stream, _) = listener.accept().await?;
            let d = daemon.clone();
            let t = token.clone();
            tokio::spawn(async move {
                if let Err(e) = serve(d, stream, t).await {
                    log::debug!("session ended: {e:#}");
                }
            });
        }
    })
}

// the single getUpdates loop. every tap that matches a waiting question wakes
// whichever session asked it.
async fn poll_updates(d: Arc<Daemon>) {
    let mut offset = 0i64;
    loop {
        let updates = match d.tg.get_updates(offset).await {
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
                handle_message(&d, msg).await;
                continue;
            }

            let Some(query) = update.callback_query else {
                continue;
            };
            if !d.cfg.allowed_user_ids.contains(&query.from.id) {
                log::warn!("ignoring a button tap from {}", query.from.id);
                continue;
            }
            let Some(msg_id) = query.message.as_ref().map(|m| m.message_id) else {
                continue;
            };
            let Some(index) = query.data.as_deref().and_then(|d| d.parse::<usize>().ok()) else {
                continue;
            };

            if let Err(e) = d.tg.answer_callback(&query.id).await {
                log::debug!("answerCallbackQuery failed: {e:#}");
            }

            if let Some(waiter) = d.pending.lock().await.remove(&msg_id) {
                let _ = waiter.send(Answer::Tapped(index));
            }
        }
    }
}

// a typed reply answers a question: whichever one it replies to, or the only
// one waiting if it replies to nothing
async fn handle_message(d: &Arc<Daemon>, msg: crate::telegram::IncomingMessage) {
    let Some(from) = msg.from else { return };
    if !d.cfg.allowed_user_ids.contains(&from.id) {
        log::warn!("ignoring a message from {}", from.id);
        return;
    }
    let Some(text) = msg.text else { return };

    let mut pending = d.pending.lock().await;
    let target = match msg.reply_to_message {
        Some(r) if pending.contains_key(&r.message_id) => Some(r.message_id),
        Some(_) => None,
        // no reply and exactly one question waiting is unambiguous enough
        None if pending.len() == 1 => pending.keys().next().copied(),
        None => None,
    };

    let Some(msg_id) = target else {
        log::debug!("nothing to route a typed reply to");
        return;
    };
    if let Some(waiter) = pending.remove(&msg_id) {
        let _ = waiter.send(Answer::Typed(text));
    }
}

async fn serve(d: Arc<Daemon>, stream: TcpStream, token: String) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    // first line has to be a hello with the right token
    let first = lines.next_line().await?.context("client sent nothing")?;
    let label = match serde_json::from_str::<Request>(&first)? {
        Request::Hello { token: t, label } if t == token => label,
        Request::Hello { .. } => anyhow::bail!("bad token"),
        _ => anyhow::bail!("expected hello"),
    };
    log::info!("session connected: {label}");

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
                match d.tg.send_message(d.cfg.chat_id, &decorate(&label, &text)).await {
                    Ok(_) => {
                        thinking = None;
                        "sent".into()
                    }
                    Err(e) => format!("could not send: {e:#}"),
                }
            }
            Request::Thinking { text } => {
                let body = decorate(&label, &format!("💭 {text}"));
                let mut out = None;
                if let Some(id) = thinking {
                    match d.tg.edit_message(d.cfg.chat_id, id, &body).await {
                        Ok(()) => out = Some("updated".to_string()),
                        Err(e) => log::debug!("status edit failed, sending a new one: {e:#}"),
                    }
                }
                match out {
                    Some(o) => o,
                    None => match d.tg.send_message(d.cfg.chat_id, &body).await {
                        Ok(id) => {
                            thinking = Some(id);
                            "sent".into()
                        }
                        Err(e) => format!("could not send: {e:#}"),
                    },
                }
            }
            Request::Ask { question, options } => {
                if options.is_empty() {
                    "need at least one option".to_string()
                } else if options.len() > MAX_OPTIONS {
                    format!("too many options — {MAX_OPTIONS} at most")
                } else {
                    match ask(&d, &label, &question, &options).await {
                        Ok(answer) => answer,
                        Err(e) => format!("could not ask: {e:#}"),
                    }
                }
            }
        };

        let resp = Response { result };
        write.write_all(format!("{}\n", serde_json::to_string(&resp)?).as_bytes()).await?;
    }

    log::info!("session gone: {label}");
    Ok(())
}

async fn ask(d: &Daemon, label: &str, question: &str, options: &[String]) -> Result<String> {
    let chat = d.cfg.chat_id;
    let text = decorate(label, question);
    let prompt = format!("{text}\n\n(or reply to this message with your own answer)");
    let msg_id = d.tg.send_with_buttons(chat, &prompt, options).await?;

    let (tx, rx) = oneshot::channel();
    d.pending.lock().await.insert(msg_id, tx);

    let wait = Duration::from_secs(d.cfg.ask_timeout_seconds);
    let picked = tokio::time::timeout(wait, rx).await;

    // whatever happened, this question is no longer waiting
    d.pending.lock().await.remove(&msg_id);

    match picked {
        Ok(Ok(Answer::Tapped(index))) => {
            let answer = options
                .get(index)
                .cloned()
                .unwrap_or_else(|| "unknown option".into());
            let settled = format!("{text}\n\n✅ {}. {answer}", index + 1);
            if let Err(e) = d.tg.edit_message(chat, msg_id, &settled).await {
                log::debug!("could not clear the keyboard: {e:#}");
            }
            Ok(answer)
        }
        Ok(Ok(Answer::Typed(typed))) => {
            let settled = format!("{text}\n\n✅ {typed}");
            if let Err(e) = d.tg.edit_message(chat, msg_id, &settled).await {
                log::debug!("could not clear the keyboard: {e:#}");
            }
            Ok(typed)
        }
        // the sender was dropped, which shouldn't happen but isn't fatal
        Ok(Err(_)) => Ok("no answer — the question was cancelled".into()),
        Err(_) => {
            let timed_out = format!("{text}\n\n⏱ no answer after {}s", d.cfg.ask_timeout_seconds);
            if let Err(e) = d.tg.edit_message(chat, msg_id, &timed_out).await {
                log::debug!("could not clear the keyboard: {e:#}");
            }
            Ok(format!(
                "no answer — the user did not pick an option within {}s",
                d.cfg.ask_timeout_seconds
            ))
        }
    }
}

// so you can tell which project is talking when more than one is
fn decorate(label: &str, text: &str) -> String {
    format!("📁 {label}\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_goes_above_the_text() {
        assert_eq!(decorate("telepager", "hello"), "📁 telepager\nhello");
    }
}
