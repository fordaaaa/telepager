//a thin telegram bot api client. just the handful of calls this server needs:
//send a message, edit one in place, attach an inline keyboard, acknowledge a
//button tap, and long-poll for updates. no framework, just json over https.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

//telegram rejects messages over 4096 characters
pub const MAX_MESSAGE_LEN: usize = 4096;

//how long getUpdates holds the connection open waiting for something to happen
const POLL_SECONDS: u64 = 25;

pub struct Telegram {
    http: reqwest::Client,
    base: String,
    //kept so errors can be scrubbed of it before they escape this module
    token: String,
}

//the envelope every bot api response comes wrapped in
#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub data: Option<String>,
    pub message: Option<SentMessage>,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

impl Telegram {
    pub fn new(token: &str) -> Result<Self> {
        //the timeout has to outlast a long poll or every poll would be an error
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(POLL_SECONDS + 15))
            .build()
            .context("building the http client")?;
        Ok(Self {
            http,
            base: format!("https://api.telegram.org/bot{token}"),
            token: token.to_string(),
        })
    }

    //the bot token sits in the url path, and reqwest prints the url in its
    //error text. every error leaving this client goes through here first, or a
    //single network blip would write the token into a log line and into the mcp
    //tool result the model reads back.
    fn scrub(&self, e: anyhow::Error) -> anyhow::Error {
        let text = format!("{e:#}");
        if !text.contains(&self.token) {
            return e;
        }
        anyhow::anyhow!(text.replace(&self.token, "<redacted>"))
    }

    //post a json body to a bot api method and unwrap the envelope. the error
    //carries telegram's own description, which is usually the useful part.
    async fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: Value) -> Result<T> {
        self.call_inner(method, body).await.map_err(|e| self.scrub(e))
    }

    async fn call_inner<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        body: Value,
    ) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("calling {method}"))?;

        let parsed: ApiResponse<T> = resp
            .json()
            .await
            .with_context(|| format!("decoding the {method} response"))?;

        if !parsed.ok {
            bail!(
                "telegram rejected {method}: {}",
                parsed.description.unwrap_or_else(|| "no reason given".into())
            );
        }
        parsed
            .result
            .ok_or_else(|| anyhow::anyhow!("{method} returned ok with no result"))
    }

    //send one message. long text is split across several, and the id of the
    //last one comes back so a caller can keep editing it.
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64> {
        let mut last = None;
        for part in chunk_text(text, MAX_MESSAGE_LEN) {
            let sent: SentMessage = self
                .call("sendMessage", json!({ "chat_id": chat_id, "text": part }))
                .await?;
            last = Some(sent.message_id);
        }
        last.ok_or_else(|| anyhow::anyhow!("nothing to send"))
    }

    //replace the text of a message already in the chat. omitting reply_markup
    //also strips any inline keyboard it had.
    pub async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": truncate(text, MAX_MESSAGE_LEN),
        });
        //the result is the edited message, which we don't need
        let _: Value = self.call("editMessageText", body).await?;
        Ok(())
    }

    //send a message with numbered buttons under it. the callback data of each
    //button is its index, so the answer maps straight back to the options list.
    pub async fn send_with_buttons(
        &self,
        chat_id: i64,
        text: &str,
        options: &[String],
    ) -> Result<i64> {
        let buttons: Vec<Value> = options
            .iter()
            .enumerate()
            .map(|(i, label)| {
                json!({
                    "text": truncate(&format!("{}. {label}", i + 1), 64),
                    "callback_data": i.to_string(),
                })
            })
            .collect();

        //three to a row keeps short labels side by side without crowding
        let rows: Vec<Vec<Value>> = buttons.chunks(3).map(|c| c.to_vec()).collect();

        let sent: SentMessage = self
            .call(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": truncate(text, MAX_MESSAGE_LEN),
                    "reply_markup": { "inline_keyboard": rows },
                }),
            )
            .await?;
        Ok(sent.message_id)
    }

    //clears the spinner telegram shows on a tapped button. best effort — a
    //failure here doesn't change the answer we already have.
    pub async fn answer_callback(&self, callback_id: &str) -> Result<()> {
        let _: Value = self
            .call("answerCallbackQuery", json!({ "callback_query_id": callback_id }))
            .await?;
        Ok(())
    }

    //long-poll for updates past `offset`. only button taps are asked for, so
    //ordinary chat messages never accumulate on telegram's side.
    pub async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": POLL_SECONDS,
                "allowed_updates": ["callback_query"],
            }),
        )
        .await
    }
}

fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    s.chars().take(limit).collect()
}

//split text into telegram-sized pieces, preferring to break on a newline near
//the end of a chunk so code and lists don't get cut mid-line.
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return vec!["(empty)".to_string()];
    }

    let mut out = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            out.push(rest.to_string());
            break;
        }

        //byte index of the limit-th character, so we never split a codepoint
        let cut = rest
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());

        //back up to the last newline, but only if it isn't miles away
        let split = match rest[..cut].rfind('\n') {
            Some(nl) if nl > cut / 2 => nl + 1,
            _ => cut,
        };

        let (head, tail) = rest.split_at(split);
        out.push(head.to_string());
        rest = tail;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_text("hello", 100), vec!["hello"]);
    }

    #[test]
    fn empty_text_gets_a_placeholder() {
        assert_eq!(chunk_text("", 100), vec!["(empty)"]);
    }

    #[test]
    fn long_text_splits_at_the_limit() {
        let chunks = chunk_text(&"a".repeat(250), 100);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 100);
    }

    #[test]
    fn splitting_prefers_a_late_newline() {
        let text = format!("{}\n{}", "a".repeat(80), "b".repeat(80));
        let chunks = chunk_text(&text, 100);
        assert_eq!(chunks[0], format!("{}\n", "a".repeat(80)));
    }

    #[test]
    fn errors_never_carry_the_token() {
        let tg = Telegram::new("123456:SECRET-TOKEN").unwrap();
        let leaky = anyhow::anyhow!("calling sendMessage")
            .context("error sending request for url (https://api.telegram.org/bot123456:SECRET-TOKEN/sendMessage)");
        let scrubbed = format!("{:#}", tg.scrub(leaky));
        assert!(!scrubbed.contains("SECRET-TOKEN"));
        assert!(scrubbed.contains("<redacted>"));
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("héllo", 3), "hél");
        assert_eq!(truncate("hi", 10), "hi");
    }
}
