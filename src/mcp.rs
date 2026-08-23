use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use tokio::sync::Mutex;

use crate::client::Client;
use crate::ipc::Request;

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
    // one connection to the daemon, tool calls take turns on it
    client: Arc<Mutex<Client>>,
    // read by the tool_handler macro, the lint just can't see it
    #[allow(dead_code)]
    tool_router: ToolRouter<Server>,
}

#[tool_router]
impl Server {
    fn new(client: Arc<Mutex<Client>>) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }

    // the daemon does the work; this carries the request over and waits.
    // block_in_place keeps the blocking socket read off the async executor.
    async fn forward(&self, req: Request) -> String {
        let mut guard = self.client.lock().await;
        tokio::task::block_in_place(|| match guard.call(&req) {
            Ok(result) => result,
            Err(e) => format!("could not reach the telepager daemon: {e:#}"),
        })
    }

    #[tool(description = "send a message to the telegram user")]
    async fn send_message(&self, Parameters(req): Parameters<MessageRequest>) -> String {
        self.forward(Request::Send { text: req.text }).await
    }

    #[tool(
        description = "show or update a status line in telegram, edited in place \
                       rather than sent as a new message. use it to keep the user \
                       posted during long work."
    )]
    async fn send_thinking(&self, Parameters(req): Parameters<ThinkingRequest>) -> String {
        self.forward(Request::Thinking { text: req.text }).await
    }

    #[tool(
        description = "ask the telegram user a question with numbered buttons and \
                       wait for their answer. returns the option they picked."
    )]
    async fn ask_question(&self, Parameters(req): Parameters<AskRequest>) -> String {
        self.forward(Request::Ask {
            question: req.question,
            options: req.options,
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        // these structs are non exhaustive so you can't just write a literal
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
    // fail here rather than on the first tool call, so a bad config shows up
    // when the client starts us
    crate::config::load(config.as_deref())?;
    let client = Arc::new(Mutex::new(Client::connect(config.as_ref())?));

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let service = Server::new(client).serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
