//the mcp server. three tools, all of them ways of paging one telegram user:
//say something, show a status line that updates in place, and ask a question
//with numbered buttons and wait for the tap.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::telegram::Telegram;

//more buttons than this is a menu, not a question
const MAX_OPTIONS: usize = 20;

struct State {
    cfg: Config,
    tg: Telegram,
    //the status message being edited in place, if one is live
    thinking: Mutex<Option<i64>>,
    //next getUpdates offset, and the lock that keeps two questions from
    //polling at once and stealing each other's answers
    offset: Mutex<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MessageRequest {
    #[schemars(description = "the text to send to the telegram user")]
    text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ThinkingRequest {
    #[schemars(description = "the current status, e.g. 'reading src/main.rs'")]
    text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AskRequest {
    #[schemars(description = "the question to ask the telegram user")]
    question: String,
    #[schemars(description = "the answers to offer, shown as numbered buttons")]
    options: Vec<String>,
}

#[derive(Clone)]
struct Server {
    st: Arc<State>,
    //read by the tool_handler macro's generated code, the lint just can't see it
    #[allow(dead_code)]
    tool_router: ToolRouter<Server>,
}

#[tool_router]
impl Server {
    fn new(st: Arc<State>) -> Self {
        Self {
            st,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "send a message to the telegram user")]
    async fn send_message(&self, Parameters(req): Parameters<MessageRequest>) -> String {
        match self.st.tg.send_message(self.st.cfg.chat_id, &req.text).await {
            Ok(_) => {
                //the status line belongs above this message now, so the next
                //update starts a fresh one at the bottom of the chat
                *self.st.thinking.lock().await = None;
                "sent".into()
            }
            Err(e) => format!("could not send: {e:#}"),
        }
    }

    #[tool(
        description = "show or update a status line in telegram, edited in place \
                       rather than sent as a new message. use it to keep the user \
                       posted during long work."
    )]
    async fn send_thinking(&self, Parameters(req): Parameters<ThinkingRequest>) -> String {
        let text = format!("💭 {}", req.text);
        let chat = self.st.cfg.chat_id;
        let mut slot = self.st.thinking.lock().await;

        //edit the live status line if there is one. an edit fails when the text
        //hasn't changed or the message is gone, and neither is worth an error —
        //fall through and start a new one.
        if let Some(id) = *slot {
            match self.st.tg.edit_message(chat, id, &text).await {
                Ok(()) => return "updated".into(),
                Err(e) => log::debug!("status edit failed, sending a new one: {e:#}"),
            }
        }

        match self.st.tg.send_message(chat, &text).await {
            Ok(id) => {
                *slot = Some(id);
                "sent".into()
            }
            Err(e) => format!("could not send: {e:#}"),
        }
    }

    #[tool(
        description = "ask the telegram user a question with numbered buttons and \
                       wait for their answer. returns the option they picked."
    )]
    async fn ask_question(&self, Parameters(req): Parameters<AskRequest>) -> String {
        if req.options.is_empty() {
            return "need at least one option".into();
        }
        if req.options.len() > MAX_OPTIONS {
            return format!("too many options — {MAX_OPTIONS} at most");
        }

        match self.ask(&req.question, &req.options).await {
            Ok(answer) => answer,
            Err(e) => format!("could not ask: {e:#}"),
        }
    }
}

impl Server {
    //post the question, then poll until the right person taps one of its
    //buttons or we run out of patience
    async fn ask(&self, question: &str, options: &[String]) -> Result<String> {
        let chat = self.st.cfg.chat_id;
        let tg = &self.st.tg;

        let msg_id = tg.send_with_buttons(chat, question, options).await?;

        //polling holds the offset for the whole wait, so a second question
        //queues behind this one instead of consuming its answer
        let mut offset = self.st.offset.lock().await;
        let deadline = Instant::now() + Duration::from_secs(self.st.cfg.ask_timeout_seconds);

        while Instant::now() < deadline {
            let updates = match tg.get_updates(*offset).await {
                Ok(u) => u,
                //a dropped long poll is routine; wait a beat and try again
                Err(e) => {
                    log::debug!("getUpdates failed, retrying: {e:#}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            for update in updates {
                //acknowledge every update we see, even ones we ignore, or
                //telegram would hand them back forever
                *offset = update.update_id + 1;

                let Some(query) = update.callback_query else {
                    continue;
                };
                //a tap on an older question, or from someone not on the list
                if query.message.as_ref().map(|m| m.message_id) != Some(msg_id) {
                    continue;
                }
                if !self.st.cfg.allowed_user_ids.contains(&query.from.id) {
                    log::warn!("ignoring a button tap from {}", query.from.id);
                    continue;
                }

                let Some((index, answer)) = query
                    .data
                    .as_deref()
                    .and_then(|d| d.parse::<usize>().ok())
                    .and_then(|i| options.get(i).map(|o| (i, o.clone())))
                else {
                    continue;
                };

                //stop the button's spinner, then rewrite the question to show
                //what was picked, which also clears the keyboard
                if let Err(e) = tg.answer_callback(&query.id).await {
                    log::debug!("answerCallbackQuery failed: {e:#}");
                }
                let settled = format!("{question}\n\n✅ {}. {answer}", index + 1);
                if let Err(e) = tg.edit_message(chat, msg_id, &settled).await {
                    log::debug!("could not clear the keyboard: {e:#}");
                }

                return Ok(answer);
            }
        }

        let timed_out = format!(
            "{question}\n\n⏱ no answer after {}s",
            self.st.cfg.ask_timeout_seconds
        );
        if let Err(e) = tg.edit_message(chat, msg_id, &timed_out).await {
            log::debug!("could not clear the keyboard: {e:#}");
        }
        Ok(format!(
            "no answer — the user did not pick an option within {}s",
            self.st.cfg.ask_timeout_seconds
        ))
    }
}

//the tool_handler macro wires call_tool/list_tools to the router. get_info just
//names the server and gives the client a short hint.
#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        //these structs are non-exhaustive, so tweak a default instead of a literal
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "telepager".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(
            "page the user on telegram: send them a message, keep a status line \
             updated while you work, or ask a question with buttons and wait for \
             their answer"
                .into(),
        );
        info
    }
}

pub fn run(config: Option<PathBuf>) -> Result<()> {
    let cfg = crate::config::load(config.as_deref())?;
    let tg = Telegram::new(&cfg.token)?;
    let st = Arc::new(State {
        cfg,
        tg,
        thinking: Mutex::new(None),
        offset: Mutex::new(0),
    });

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        //stdio transport: the mcp client speaks to us over stdin/stdout
        let service = Server::new(st).serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
