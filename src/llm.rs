//! One interface over the model APIs the master agent can run on.
//!
//! Anthropic, OpenAI (and everything OpenAI-shaped — openrouter, groq,
//! together, lm studio, vllm), Google Gemini and Ollama each want a different
//! request body and hand back a different response shape. The differences are
//! all in `encode`/`decode`; everything above this file works in terms of
//! [`Msg`], [`ToolDef`] and [`Turn`].

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::{MasterConfig, Provider};

const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Why the CLI backends can't reach this file: `Llm::new` refuses them.
const CLI_GUARD: &str = "cli backends are rejected by Llm::new";
const REQUEST_TIMEOUT_SECONDS: u64 = 180;

/// A tool the model may call, described the same way for every provider.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// A JSON Schema object for the arguments.
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// One entry of conversation history.
#[derive(Debug, Clone)]
pub enum Msg {
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    ToolResult { id: String, name: String, content: String },
}

/// What the model said this turn: some prose, and any tools it wants run.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<ToolCall>,
}

pub struct Llm {
    http: reqwest::Client,
    provider: Provider,
    model: String,
    base: String,
    key: Option<String>,
    max_tokens: u32,
}

impl Llm {
    pub fn new(cfg: &MasterConfig) -> Result<Self> {
        if cfg.provider.is_cli() {
            bail!(
                "{} is a cli backend — it runs its own agent loop and never comes \
                 through here",
                cfg.provider.as_str()
            );
        }
        let key = cfg.resolve_key();
        if cfg.provider.needs_key() && key.is_none() {
            bail!(
                "no api key for {}. set {} in your environment, or put api_key \
                 in the master block of your telepager config.",
                cfg.provider.as_str(),
                cfg.provider.default_key_env()
            );
        }

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
            .build()
            .context("building the http client")?;

        Ok(Self {
            http,
            provider: cfg.provider,
            model: cfg.model_or_default(),
            base: cfg.base_url_or_default(),
            key,
            max_tokens: if cfg.max_tokens == 0 { 4096 } else { cfg.max_tokens },
        })
    }

    pub fn describe(&self) -> String {
        format!("{}/{}", self.provider.as_str(), self.model)
    }

    /// Keys leak through reqwest's error messages when they ride in the url,
    /// which is exactly what Gemini does.
    fn scrub(&self, e: anyhow::Error) -> anyhow::Error {
        let Some(key) = self.key.as_deref().filter(|k| k.len() > 6) else {
            return e;
        };
        let text = format!("{e:#}");
        if !text.contains(key) {
            return e;
        }
        anyhow::anyhow!(text.replace(key, "<redacted>"))
    }

    pub async fn turn(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        self.turn_inner(system, history, tools)
            .await
            .map_err(|e| self.scrub(e))
    }

    async fn turn_inner(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        let body = self.encode(system, history, tools);
        let (url, headers) = self.endpoint();

        let mut req = self.http.post(&url).json(&body);
        for (name, value) in headers {
            req = req.header(name, value);
        }

        let resp = req.send().await.context("calling the model")?;
        let status = resp.status();
        let text = resp.text().await.context("reading the model response")?;

        if !status.is_success() {
            bail!("{} returned {}: {}", self.provider.as_str(), status.as_u16(), first_line(&text, 400));
        }

        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("the model returned something that isn't JSON: {}", first_line(&text, 200)))?;

        self.decode(&value)
    }

    fn endpoint(&self) -> (String, Vec<(String, String)>) {
        match self.provider {
            Provider::Anthropic => (
                format!("{}/v1/messages", self.base),
                vec![
                    ("x-api-key".into(), self.key.clone().unwrap_or_default()),
                    ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
                ],
            ),
            Provider::Openai | Provider::Ollama => {
                let mut headers = Vec::new();
                if let Some(k) = &self.key {
                    headers.push(("authorization".into(), format!("Bearer {k}")));
                }
                (format!("{}/chat/completions", self.base), headers)
            }
            Provider::ClaudeCode | Provider::Opencode => unreachable!("{CLI_GUARD}"),
            Provider::Gemini => {
                // the key goes in a header rather than the query string so it
                // stays out of logs and error messages
                let headers = match &self.key {
                    Some(k) => vec![("x-goog-api-key".to_string(), k.clone())],
                    None => Vec::new(),
                };
                (
                    format!("{}/models/{}:generateContent", self.base, self.model),
                    headers,
                )
            }
        }
    }

    fn encode(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Value {
        match self.provider {
            Provider::Anthropic => encode_anthropic(&self.model, self.max_tokens, system, history, tools),
            Provider::Openai | Provider::Ollama => encode_openai(&self.model, self.max_tokens, system, history, tools),
            Provider::Gemini => encode_gemini(self.max_tokens, system, history, tools),
            Provider::ClaudeCode | Provider::Opencode => unreachable!("{CLI_GUARD}"),
        }
    }

    fn decode(&self, value: &Value) -> Result<Turn> {
        match self.provider {
            Provider::Anthropic => decode_anthropic(value),
            Provider::Openai | Provider::Ollama => decode_openai(value),
            Provider::Gemini => decode_gemini(value),
            Provider::ClaudeCode | Provider::Opencode => unreachable!("{CLI_GUARD}"),
        }
    }
}

// ---------------------------------------------------------------- anthropic

fn encode_anthropic(
    model: &str,
    max_tokens: u32,
    system: &str,
    history: &[Msg],
    tools: &[ToolDef],
) -> Value {
    let mut messages = Vec::new();

    for msg in history {
        match msg {
            Msg::User(text) => messages.push(json!({
                "role": "user",
                "content": [{ "type": "text", "text": text }],
            })),
            Msg::Assistant { text, calls } => {
                let mut content = Vec::new();
                if !text.trim().is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                for call in calls {
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.args,
                    }));
                }
                // a turn with neither prose nor calls isn't a legal message
                if !content.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": content }));
                }
            }
            Msg::ToolResult { id, content, .. } => {
                // tool results are user-role blocks here, and consecutive ones
                // have to share a single message
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": content,
                });
                match messages.last_mut() {
                    Some(last) if is_anthropic_tool_result_message(last) => {
                        if let Some(arr) = last["content"].as_array_mut() {
                            arr.push(block);
                        }
                    }
                    _ => messages.push(json!({ "role": "user", "content": [block] })),
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
    });
    if !system.trim().is_empty() {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools
            .iter()
            .map(|t| json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.schema,
            }))
            .collect::<Vec<_>>());
    }
    body
}

fn is_anthropic_tool_result_message(msg: &Value) -> bool {
    msg["role"] == "user"
        && msg["content"]
            .as_array()
            .and_then(|a| a.first())
            .map(|b| b["type"] == "tool_result")
            .unwrap_or(false)
}

fn decode_anthropic(value: &Value) -> Result<Turn> {
    let blocks = value["content"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("anthropic response had no content array"))?;

    let mut turn = Turn::default();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => push_text(&mut turn.text, block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => turn.calls.push(ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: block["name"].as_str().unwrap_or_default().to_string(),
                args: block["input"].clone(),
            }),
            _ => {}
        }
    }
    Ok(turn)
}

// ------------------------------------------------------------------- openai

fn encode_openai(
    model: &str,
    max_tokens: u32,
    system: &str,
    history: &[Msg],
    tools: &[ToolDef],
) -> Value {
    let mut messages = Vec::new();
    if !system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }

    for msg in history {
        match msg {
            Msg::User(text) => messages.push(json!({ "role": "user", "content": text })),
            Msg::Assistant { text, calls } => {
                // openai requires content unless there are tool calls, so an
                // entirely empty turn would make the request invalid
                if text.trim().is_empty() && calls.is_empty() {
                    continue;
                }
                let mut m = json!({ "role": "assistant" });
                // content has to be present even when it's only tool calls
                m["content"] = if text.trim().is_empty() { Value::Null } else { json!(text) };
                if !calls.is_empty() {
                    m["tool_calls"] = json!(calls
                        .iter()
                        .map(|c| json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                // openai wants the arguments as a json string
                                "arguments": serde_json::to_string(&c.args).unwrap_or_else(|_| "{}".into()),
                            },
                        }))
                        .collect::<Vec<_>>());
                }
                messages.push(m);
            }
            Msg::ToolResult { id, name, content } => messages.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "name": name,
                "content": content,
            })),
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema,
                },
            }))
            .collect::<Vec<_>>());
    }
    body
}

fn decode_openai(value: &Value) -> Result<Turn> {
    // an error body can come back with a 200 from some compatible servers
    if let Some(err) = value["error"]["message"].as_str() {
        bail!("the model returned an error: {err}");
    }

    let message = value["choices"]
        .as_array()
        .and_then(|c| c.first())
        .map(|c| &c["message"])
        .ok_or_else(|| anyhow::anyhow!("the response had no choices"))?;

    let mut turn = Turn::default();
    if let Some(text) = message["content"].as_str() {
        turn.text = text.to_string();
    }

    if let Some(calls) = message["tool_calls"].as_array() {
        for (i, call) in calls.iter().enumerate() {
            let name = call["function"]["name"].as_str().unwrap_or_default().to_string();
            // arguments are a string of json, and a model can emit a broken one
            let args = match call["function"]["arguments"].as_str() {
                Some(raw) if !raw.trim().is_empty() => {
                    serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
                }
                _ => call["function"]["arguments"].clone(),
            };
            let id = call["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}"));
            turn.calls.push(ToolCall { id, name, args: normalise_args(args) });
        }
    }
    Ok(turn)
}

// ------------------------------------------------------------------- gemini

fn encode_gemini(max_tokens: u32, system: &str, history: &[Msg], tools: &[ToolDef]) -> Value {
    let mut contents: Vec<Value> = Vec::new();

    for msg in history {
        match msg {
            Msg::User(text) => contents.push(json!({
                "role": "user",
                "parts": [{ "text": text }],
            })),
            Msg::Assistant { text, calls } => {
                let mut parts = Vec::new();
                if !text.trim().is_empty() {
                    parts.push(json!({ "text": text }));
                }
                for call in calls {
                    parts.push(json!({
                        "functionCall": { "name": call.name, "args": call.args },
                    }));
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Msg::ToolResult { name, content, .. } => {
                // gemini has no call ids, it matches results to calls by name
                let part = json!({
                    "functionResponse": {
                        "name": name,
                        "response": { "result": content },
                    },
                });
                match contents.last_mut() {
                    Some(last) if last["role"] == "user" && has_function_response(last) => {
                        if let Some(arr) = last["parts"].as_array_mut() {
                            arr.push(part);
                        }
                    }
                    _ => contents.push(json!({ "role": "user", "parts": [part] })),
                }
            }
        }
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": { "maxOutputTokens": max_tokens },
    });
    if !system.trim().is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if !tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": strip_schema_for_gemini(&t.schema),
                }))
                .collect::<Vec<_>>(),
        }]);
    }
    body
}

fn has_function_response(content: &Value) -> bool {
    content["parts"]
        .as_array()
        .and_then(|a| a.first())
        .map(|p| p.get("functionResponse").is_some())
        .unwrap_or(false)
}

/// Gemini's schema dialect rejects the JSON Schema keywords the other two
/// ignore, so drop the ones we might emit.
fn strip_schema_for_gemini(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if matches!(k.as_str(), "$schema" | "additionalProperties" | "default" | "examples") {
                    continue;
                }
                out.insert(k.clone(), strip_schema_for_gemini(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip_schema_for_gemini).collect()),
        other => other.clone(),
    }
}

fn decode_gemini(value: &Value) -> Result<Turn> {
    if let Some(err) = value["error"]["message"].as_str() {
        bail!("the model returned an error: {err}");
    }

    let parts = value["candidates"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["content"]["parts"].as_array())
        .cloned()
        .unwrap_or_default();

    let mut turn = Turn::default();
    for (i, part) in parts.iter().enumerate() {
        if let Some(text) = part["text"].as_str() {
            push_text(&mut turn.text, text);
        }
        if let Some(call) = part.get("functionCall") {
            let name = call["name"].as_str().unwrap_or_default().to_string();
            turn.calls.push(ToolCall {
                // synthesised, since gemini matches results to calls by name
                id: format!("{name}_{i}"),
                name,
                args: normalise_args(call["args"].clone()),
            });
        }
    }
    Ok(turn)
}

// -------------------------------------------------------------------- utils

fn push_text(buf: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(text);
}

/// Small models sometimes send arguments as a json string, or as null.
fn normalise_args(args: Value) -> Value {
    match args {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Object(Default::default())),
        Value::Null => Value::Object(Default::default()),
        other => other,
    }
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

    fn tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "spawn_agent".into(),
            description: "start one".into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "dir": { "type": "string" } },
                "required": ["dir"],
            }),
        }]
    }

    fn history() -> Vec<Msg> {
        vec![
            Msg::User("start something".into()),
            Msg::Assistant {
                text: "on it".into(),
                calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "spawn_agent".into(),
                    args: json!({ "dir": "/tmp" }),
                }],
            },
            Msg::ToolResult {
                id: "call_1".into(),
                name: "spawn_agent".into(),
                content: "started".into(),
            },
        ]
    }

    #[test]
    fn anthropic_puts_tool_results_in_a_user_message() {
        let body = encode_anthropic("m", 100, "sys", &history(), &tools());
        assert_eq!(body["system"], "sys");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn anthropic_merges_consecutive_tool_results() {
        let h = vec![
            Msg::Assistant { text: String::new(), calls: vec![] },
            Msg::ToolResult { id: "a".into(), name: "t".into(), content: "1".into() },
            Msg::ToolResult { id: "b".into(), name: "t".into(), content: "2".into() },
        ];
        let body = encode_anthropic("m", 100, "", &h, &[]);
        let messages = body["messages"].as_array().unwrap();
        // the empty assistant turn is dropped, the two results share a message
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn openai_drops_an_entirely_empty_assistant_turn() {
        let h = vec![
            Msg::User("hi".into()),
            // what a provider returns on a length cap or a filtered response
            Msg::Assistant { text: String::new(), calls: vec![] },
            Msg::User("still there?".into()),
        ];
        let body = encode_openai("m", 100, "", &h, &[]);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|m| m["role"] != "assistant"));
    }

    #[test]
    fn openai_serialises_arguments_as_a_string() {
        let body = encode_openai("m", 100, "sys", &history(), &tools());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        let args = messages[2]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert_eq!(args, r#"{"dir":"/tmp"}"#);
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
    }

    #[test]
    fn gemini_strips_schema_keywords_it_rejects() {
        let body = encode_gemini(100, "sys", &history(), &tools());
        let params = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(params.get("additionalProperties").is_none());
        assert_eq!(params["type"], "object");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(body["contents"][1]["role"], "model");
        assert!(body["contents"][2]["parts"][0].get("functionResponse").is_some());
    }

    #[test]
    fn anthropic_response_splits_text_from_calls() {
        let turn = decode_anthropic(&json!({
            "content": [
                { "type": "text", "text": "thinking" },
                { "type": "tool_use", "id": "x", "name": "spawn_agent", "input": { "dir": "/a" } }
            ]
        }))
        .unwrap();
        assert_eq!(turn.text, "thinking");
        assert_eq!(turn.calls.len(), 1);
        assert_eq!(turn.calls[0].args["dir"], "/a");
    }

    #[test]
    fn openai_response_parses_stringified_arguments() {
        let turn = decode_openai(&json!({
            "choices": [{ "message": {
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "function": { "name": "spawn_agent", "arguments": "{\"dir\":\"/a\"}" }
                }]
            }}]
        }))
        .unwrap();
        assert_eq!(turn.text, "");
        assert_eq!(turn.calls[0].id, "c1");
        assert_eq!(turn.calls[0].args["dir"], "/a");
    }

    #[test]
    fn openai_response_survives_broken_arguments() {
        let turn = decode_openai(&json!({
            "choices": [{ "message": {
                "tool_calls": [{ "id": "c1", "function": { "name": "t", "arguments": "{not json" } }]
            }}]
        }))
        .unwrap();
        assert_eq!(turn.calls[0].args, json!({}));
    }

    #[test]
    fn openai_missing_call_ids_get_synthesised() {
        let turn = decode_openai(&json!({
            "choices": [{ "message": {
                "tool_calls": [{ "function": { "name": "t", "arguments": "{}" } }]
            }}]
        }))
        .unwrap();
        assert_eq!(turn.calls[0].id, "call_0");
    }

    #[test]
    fn gemini_response_synthesises_call_ids() {
        let turn = decode_gemini(&json!({
            "candidates": [{ "content": { "parts": [
                { "text": "sure" },
                { "functionCall": { "name": "spawn_agent", "args": { "dir": "/a" } } }
            ]}}]
        }))
        .unwrap();
        assert_eq!(turn.text, "sure");
        assert_eq!(turn.calls[0].name, "spawn_agent");
        assert!(!turn.calls[0].id.is_empty());
    }

    #[test]
    fn an_error_body_with_a_200_is_still_an_error() {
        assert!(decode_openai(&json!({ "error": { "message": "no credit" } })).is_err());
        assert!(decode_gemini(&json!({ "error": { "message": "bad key" } })).is_err());
    }

    #[test]
    fn arguments_that_arrive_as_a_string_are_parsed() {
        assert_eq!(normalise_args(json!("{\"a\":1}"))["a"], 1);
        assert_eq!(normalise_args(Value::Null), json!({}));
    }

    #[test]
    fn keys_never_appear_in_errors() {
        let cfg = MasterConfig {
            provider: Provider::Gemini,
            api_key: Some("SUPER-SECRET-KEY".into()),
            ..MasterConfig::default()
        };
        let llm = Llm::new(&cfg).unwrap();
        let leaky = anyhow::anyhow!("calling https://x/models?key=SUPER-SECRET-KEY");
        assert!(!format!("{:#}", llm.scrub(leaky)).contains("SUPER-SECRET-KEY"));
    }

    #[test]
    fn a_missing_key_is_caught_before_any_request() {
        let cfg = MasterConfig {
            provider: Provider::Anthropic,
            api_key_env: Some("TELEPAGER_TEST_KEY_THAT_IS_NOT_SET".into()),
            ..MasterConfig::default()
        };
        // only fails when the real env vars are absent too
        if cfg.resolve_key().is_none() {
            assert!(Llm::new(&cfg).is_err());
        }
    }

    #[test]
    fn gemini_puts_the_key_in_a_header_not_the_url() {
        let cfg = MasterConfig {
            provider: Provider::Gemini,
            api_key: Some("K".into()),
            ..MasterConfig::default()
        };
        let (url, headers) = Llm::new(&cfg).unwrap().endpoint();
        assert!(!url.contains("K="), "{url}");
        assert!(headers.iter().any(|(n, v)| n == "x-goog-api-key" && v == "K"));
    }
}
