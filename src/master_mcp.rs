//! The tools a CLI-backed master agent reaches telepager through.
//!
//! When the master agent runs on Claude Code or opencode rather than a raw
//! API, the agent loop lives inside that CLI. It still needs telepager's tools
//! — start an agent, list sessions, kill one — so telepager hands it an MCP
//! server: this one, spawned as `telepager master-mcp`.
//!
//! It owns no state. Every call is proxied to the running app's console API,
//! so there is still exactly one implementation of each tool.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map, Value};

use crate::master;

/// Which surface the person driving this agent is on. Telegram is remote and
/// only reaches allowlisted directories; the console isn't restricted.
const ORIGIN_ENV: &str = "TELEPAGER_ORIGIN";

#[derive(Clone)]
struct Bridge {
    http: reqwest::Client,
    base: String,
    key: String,
    origin: String,
}

impl Bridge {
    async fn call(&self, name: &str, args: Value) -> Result<String> {
        let resp = self
            .http
            .post(format!("{}/api/tool", self.base))
            .header("x-telepager-key", &self.key)
            .json(&json!({ "name": name, "args": args, "origin": self.origin }))
            .send()
            .await
            .context("reaching the telepager app")?;

        let body: Value = resp.json().await.context("decoding the app's reply")?;
        if let Some(error) = body["error"].as_str() {
            anyhow::bail!("{error}");
        }
        Ok(body["result"].as_str().unwrap_or_default().to_string())
    }
}

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "telepager".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(master::system_prompt(None));
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = master::tools()
            .into_iter()
            .map(|t| {
                let schema: Map<String, Value> = match t.schema {
                    Value::Object(map) => map,
                    _ => Map::new(),
                };
                Tool::new(
                    Cow::Owned(t.name),
                    Cow::Owned(t.description),
                    Arc::new(schema),
                )
            })
            .collect();

        Ok(ListToolsResult { tools, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));

        // a tool that failed is something the agent should read and react to,
        // not a protocol error that kills its turn
        let text = match self.call(&request.name, args).await {
            Ok(text) => text,
            Err(e) => format!("That didn't work: {e:#}"),
        };

        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
}

pub fn run() -> Result<()> {
    let endpoint = crate::client::running_endpoint()
        .context("the telepager app isn't running — start it with `telepager`")?;
    let port = endpoint
        .ui_port
        .context("the app is running but published no console address")?;
    let key = endpoint
        .ui_key
        .context("the app is running but published no console key")?;

    let bridge = Bridge {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
        key,
        origin: std::env::var(ORIGIN_ENV).unwrap_or_else(|_| "ui".into()),
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let service = bridge.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_master_tool_survives_the_conversion_to_mcp() {
        let defs = master::tools();
        let names: Vec<String> = defs.iter().map(|t| t.name.clone()).collect();

        for def in defs {
            let schema: Map<String, Value> = match def.schema {
                Value::Object(map) => map,
                _ => Map::new(),
            };
            // an empty schema would make the tool uncallable from some clients
            assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));
        }

        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"list_sessions".to_string()));
    }
}
