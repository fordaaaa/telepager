use std::io::Write;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_MESSAGE_LEN: usize = 4096;
/// Largest telegram file we will save to the inbox. Anything bigger is
/// rejected with a reply rather than written to disk.
pub const MAX_FILE_SIZE: usize = 20 * 1024 * 1024;
const POLL_SECONDS: u64 = 25;

pub struct Telegram {
    http: reqwest::Client,
    base: String,
    token: String,
}

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
    pub message: Option<IncomingMessage>,
}

#[derive(Debug, Deserialize)]
pub struct IncomingMessage {
    #[serde(default)]
    pub message_id: i64,
    /// Telegram's unix timestamp for when the message was sent. Used to drop
    /// replayed/stale updates (see `is_stale`).
    #[serde(default)]
    pub date: Option<i64>,
    pub chat: Option<Chat>,
    pub from: Option<User>,
    pub text: Option<String>,
    pub reply_to_message: Option<SentMessage>,
    /// Photo variants smallest-first; the last one is the largest.
    #[serde(default)]
    pub photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    pub voice: Option<Voice>,
    /// An audio message (a file, as opposed to a voice note). Same shape.
    #[serde(default)]
    pub audio: Option<Voice>,
    /// A circular video note. Same shape; `length` aliases to `duration`.
    #[serde(default)]
    pub video_note: Option<Voice>,
    #[serde(default)]
    pub document: Option<Document>,
    #[serde(default)]
    pub caption: Option<String>,
}

/// Messages older than this are replay, not news: drop them.
pub const MAX_MESSAGE_AGE_SECS: i64 = 120;

/// Whether a message dated `date` is stale at `now` (both unix seconds).
/// Missing/zero dates (old payloads, hand-built test messages) are never stale.
pub fn is_stale(date: Option<i64>, now: i64) -> bool {
    match date {
        Some(d) if d > 0 => now - d > MAX_MESSAGE_AGE_SECS,
        _ => false,
    }
}

#[allow(dead_code)] // parsed for shape/validation; width/height aren't read yet
#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    pub file_id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default, alias = "length")]
    pub duration: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    pub file_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

impl IncomingMessage {
    /// Where this message is, so it can be taken back down again.
    pub fn location(&self) -> Option<(i64, i64)> {
        let chat = self.chat.as_ref().map(|c| c.id).or_else(|| self.from.as_ref().map(|u| u.id))?;
        (self.message_id != 0).then_some((chat, self.message_id))
    }

    /// Whether this message is a replay: sent more than
    /// `MAX_MESSAGE_AGE_SECS` before `now`.
    pub fn is_stale(&self, now: i64) -> bool {
        is_stale(self.date, now)
    }

    /// The largest photo variant — telegram sends them smallest-first, so the
    /// last entry is the best copy.
    pub fn largest_photo(&self) -> Option<&PhotoSize> {
        self.photo.as_ref().and_then(|sizes| sizes.last())
    }
}

/// What a telegram `getFile` answer carries.
#[derive(Debug, Deserialize)]
pub struct FileInfo {
    pub file_path: String,
    #[serde(default)]
    pub file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// Telegram's `type`: "private" | "group" | "supergroup" | "channel".
    /// /key refuses groups, so keys never land in shared chats.
    #[serde(default, rename = "type")]
    pub typ: Option<String>,
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
    pub username: Option<String>,
    pub first_name: Option<String>,
}

impl User {
    // something to show next to the id in the setup page
    pub fn display_name(&self) -> String {
        if let Some(u) = &self.username {
            return format!("@{u}");
        }
        self.first_name.clone().unwrap_or_else(|| self.id.to_string())
    }
}

impl Telegram {
    pub fn new(token: &str) -> Result<Self> {
        // has to outlast a long poll or every poll is an error
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

    /// Point the client somewhere other than telegram, so a test can stand a
    /// fake api up on localhost.
    #[cfg(test)]
    pub fn with_base(base: &str) -> Self {
        Self {
            // a fake api that wedges should fail the test, not hang it
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("building the http client"),
            base: base.to_string(),
            token: "test".into(),
        }
    }

    // reqwest prints the request url in its errors and the token is in the url
    fn scrub(&self, e: anyhow::Error) -> anyhow::Error {
        let text = format!("{e:#}");
        let encoded = percent_encode(&self.token);
        if !text.contains(&self.token) && (encoded.is_empty() || !text.contains(&encoded)) {
            return e;
        }
        let mut out = text.replace(&self.token, "<redacted>");
        if !encoded.is_empty() && encoded != self.token {
            out = out.replace(&encoded, "<redacted>");
        }
        anyhow::anyhow!(out)
    }

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

    // the cheapest way to find out whether a token is real
    pub async fn get_me(&self) -> Result<User> {
        self.call("getMe", json!({})).await
    }

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

    // leaving out reply_markup also clears any keyboard the message had
    pub async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": truncate(text, MAX_MESSAGE_LEN),
        });
        match self.call::<Value>("editMessageText", body).await {
            Ok(_) => Ok(()),
            // editing a message into the text it already has isn't a real failure
            Err(e) if is_unmodified(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let _: Value = self
            .call("deleteMessage", json!({ "chat_id": chat_id, "message_id": message_id }))
            .await?;
        Ok(())
    }

    /// `question_id` rides in the callback data so a tap identifies its
    /// question, rather than relying on the message being unique.
    pub async fn send_with_buttons(
        &self,
        chat_id: i64,
        text: &str,
        options: &[String],
        question_id: &str,
    ) -> Result<i64> {
        let buttons: Vec<Value> = options
            .iter()
            .enumerate()
            .map(|(i, label)| {
                json!({
                    "text": truncate(&format!("{}. {label}", i + 1), 64),
                    // telegram caps callback_data at 64 bytes
                    "callback_data": truncate(&format!("{question_id}:{i}"), 64),
                })
            })
            .collect();

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

    pub async fn answer_callback(&self, callback_id: &str) -> Result<()> {
        let _: Value = self
            .call("answerCallbackQuery", json!({ "callback_query_id": callback_id }))
            .await?;
        Ok(())
    }

    pub async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        self.get_updates_for(offset, POLL_SECONDS).await
    }

    pub async fn get_updates_for(&self, offset: i64, timeout: u64) -> Result<Vec<Update>> {
        self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": timeout,
                "allowed_updates": ["callback_query", "message"],
            }),
        )
        .await
    }

    /// `getFile`: a file_id from an incoming photo/voice/document becomes the
    /// path downloads are served from, along with the size telegram declared.
    pub async fn get_file(&self, file_id: &str) -> Result<FileInfo> {
        self.call("getFile", json!({ "file_id": file_id })).await
    }

    /// Download a file `get_file` named. The bot token is in the url, so
    /// errors go through `scrub` like every other call. The declared size is
    /// refused before anything is fetched, and the Content-Length before a
    /// byte is buffered.
    pub async fn download_file(&self, file: &FileInfo) -> Result<Vec<u8>> {
        let url = file_url_for_base(&self.base, &file.file_path);
        self.download_inner(&url, file.file_size).await.map_err(|e| self.scrub(e))
    }

    async fn download_inner(&self, url: &str, declared_size: Option<u64>) -> Result<Vec<u8>> {
        check_declared_size(declared_size)?;
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .context("downloading the file")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("downloading the file returned {}", status.as_u16());
        }
        check_content_length(resp.content_length())?;
        let bytes = resp.bytes().await.context("reading the downloaded file")?;
        check_file_size(bytes.len())?;
        Ok(bytes.to_vec())
    }
}

fn is_unmodified(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("message is not modified")
}

/// Percent-encode every non-alphanumeric byte, so a token that appears in an
/// error as `123%3ASECRET` (the `:` encoded) is scrubbed too, not just the
/// raw form.
pub fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xF) as usize] as char);
        }
    }
    out
}

/// One bot token gets one poller: a second `getUpdates` terminates the first and
/// the two then trade this error while messages land on whichever poll was live.
/// Retrying never fixes it, so say it out loud rather than swallowing it.
pub fn is_conflict(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("terminated by other getUpdates request")
}

fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    s.chars().take(limit).collect()
}

// ---------------------------------------------------------- media & inbox

/// The download a media message needs: which file, what to save it as, and
/// what to tell the master about it. Derived purely from the message, so the
/// routing decision can be tested without the network.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlan {
    pub kind: &'static str,
    pub file_id: String,
    pub ext: String,
    pub caption: Option<String>,
    pub mime_type: Option<String>,
    pub duration: Option<i64>,
    pub file_name: Option<String>,
}

/// `None` for a plain text message — the caller keeps the old text routing.
/// A non-text message (photo/voice/document, even with no caption) gets a plan
/// and reaches the master instead of being dropped.
pub fn media_plan(msg: &IncomingMessage) -> Option<MediaPlan> {
    if let Some(photo) = msg.largest_photo() {
        return Some(MediaPlan {
            kind: "photo",
            file_id: photo.file_id.clone(),
            ext: ext_for_mime(None, None, "jpg"),
            caption: msg.caption.clone(),
            mime_type: None,
            duration: None,
            file_name: None,
        });
    }
    if let Some(voice) = &msg.voice {
        return Some(MediaPlan {
            kind: "voice",
            file_id: voice.file_id.clone(),
            ext: ext_for_mime(voice.mime_type.as_deref(), None, "ogg"),
            caption: msg.caption.clone(),
            mime_type: voice.mime_type.clone(),
            duration: voice.duration,
            file_name: None,
        });
    }
    if let Some(audio) = &msg.audio {
        return Some(MediaPlan {
            kind: "audio",
            file_id: audio.file_id.clone(),
            ext: ext_for_mime(audio.mime_type.as_deref(), None, "ogg"),
            caption: msg.caption.clone(),
            mime_type: audio.mime_type.clone(),
            duration: audio.duration,
            file_name: None,
        });
    }
    if let Some(note) = &msg.video_note {
        return Some(MediaPlan {
            kind: "video_note",
            file_id: note.file_id.clone(),
            ext: ext_for_mime(note.mime_type.as_deref(), None, "mp4"),
            caption: msg.caption.clone(),
            mime_type: note.mime_type.clone(),
            duration: note.duration,
            file_name: None,
        });
    }
    if let Some(doc) = &msg.document {
        return Some(MediaPlan {
            kind: "document",
            file_id: doc.file_id.clone(),
            ext: ext_for_mime(doc.mime_type.as_deref(), doc.file_name.as_deref(), "bin"),
            caption: msg.caption.clone(),
            mime_type: doc.mime_type.clone(),
            duration: None,
            file_name: doc.file_name.clone(),
        });
    }
    None
}

/// The prompt the master sees for a saved file. The model interface stays
/// text-only (`Msg::User(String)`); wiring real multimodal base64 — photo
/// bytes into the image blocks of encode_anthropic/openai/gemini — is
/// deliberately a future slice, so the master acts on the file at this path
/// (e.g. with its shell tools) instead of seeing the pixels.
pub fn media_prompt(plan: &MediaPlan, path: &Path) -> String {
    let mut prompt = format!("[{} saved at {}.", plan.kind, path.display());
    if let Some(caption) = plan.caption.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        prompt.push_str(&format!(" Caption: {caption}"));
    }
    prompt.push_str("] describe/act on it.");
    if matches!(plan.kind, "voice" | "audio") {
        let duration = plan.duration.map(|s| format!("{s}s")).unwrap_or_else(|| "unknown length".into());
        let mime = plan.mime_type.as_deref().unwrap_or("unknown mime type");
        prompt.push_str(&format!(
            " It's a {duration} {mime} voice note. No transcript is available — \
             transcribing needs a speech-to-text (STT) backend, which isn't wired up. \
             Tell the user what you can do without one, or ask them to type what they said."
        ));
    }
    prompt
}

/// Extensions a saved file may carry. Anything else falls back, so a
/// telegram-supplied file_name can never choose an executable or a path.
fn extension_allowed(ext: &str) -> bool {
    const ALLOWED: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "heic", "heif", "svg", "ico",
        "ogg", "oga", "mp3", "wav", "m4a", "opus", "flac", "aac", "webm",
        "mp4", "mov", "avi", "mkv",
        "txt", "md", "json", "csv", "log", "rtf", "pdf", "doc", "docx", "xls", "xlsx",
        "ppt", "pptx", "zip", "tar", "gz", "xz", "7z", "bin",
    ];
    !ext.is_empty() && ext.len() <= 10 && ext.chars().all(|c| c.is_ascii_alphanumeric()) && ALLOWED.contains(&ext)
}

/// Sanitise an extension candidate: lowercase, alphanumeric-only, whitelisted.
/// `None` when it isn't one — the caller supplies the fallback.
fn sanitise_ext(raw: &str) -> Option<String> {
    let ext = raw.trim().trim_start_matches('.').to_lowercase();
    extension_allowed(&ext).then_some(ext)
}

/// Pick the extension for a saved file from the mime type first, then the
/// telegram-supplied file_name (only its final whitelisted extension), then
/// the kind's default. The file_name itself is never used as a path.
pub fn ext_for_mime(mime: Option<&str>, file_name: Option<&str>, fallback: &str) -> String {
    if let Some(mime) = mime {
        let mime = mime.split(';').next().unwrap_or("").trim().to_lowercase();
        let from_mime = match mime.as_str() {
            "image/jpeg" => "jpeg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/bmp" => "bmp",
            "image/tiff" => "tiff",
            "image/heic" => "heic",
            "image/heif" => "heif",
            "audio/ogg" => "ogg",
            "audio/mpeg" | "audio/mp3" => "mp3",
            "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
            "audio/mp4" | "audio/x-m4a" => "m4a",
            "audio/webm" => "webm",
            "audio/opus" => "opus",
            "audio/flac" => "flac",
            "video/mp4" => "mp4",
            "video/quicktime" => "mov",
            "video/webm" => "webm",
            "video/x-msvideo" => "avi",
            "text/plain" => "txt",
            "text/markdown" => "md",
            "text/csv" => "csv",
            "application/json" => "json",
            "application/pdf" => "pdf",
            "application/zip" => "zip",
            "application/gzip" => "gz",
            "application/x-tar" => "tar",
            _ => "",
        };
        if let Some(ext) = sanitise_ext(from_mime) {
            return ext;
        }
    }
    if let Some(name) = file_name {
        // only the last path component's final extension — "../x.jpg" is "jpg",
        // "../../etc/passwd" has no extension and falls through
        let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if let Some((_, ext)) = leaf.rsplit_once('.') {
            if let Some(ext) = sanitise_ext(ext) {
                return ext;
            }
        }
    }
    sanitise_ext(fallback).unwrap_or_else(|| "bin".to_string())
}

/// Reject anything over the size cap before it is saved.
pub fn check_file_size(len: usize) -> Result<()> {
    if len > MAX_FILE_SIZE {
        bail!("file is too large ({len} bytes, max {MAX_FILE_SIZE})");
    }
    Ok(())
}

/// Refuse a `getFile` answer that declares more than the cap: no reason to
/// open the download at all when telegram already said it's too big. `None`
/// means telegram didn't say, so the later checks have to catch it.
pub fn check_declared_size(file_size: Option<u64>) -> Result<()> {
    let Some(size) = file_size else { return Ok(()) };
    if size > MAX_FILE_SIZE as u64 {
        bail!("file is too large ({size} bytes, max {MAX_FILE_SIZE})");
    }
    Ok(())
}

/// Refuse a download whose Content-Length already exceeds the cap, before a
/// byte of it is buffered in memory. `None` means no header was sent.
pub fn check_content_length(content_length: Option<u64>) -> Result<()> {
    check_declared_size(content_length)
}

/// Is `path` inside `base`? Component-wise, so "/inbox-evil" is not inside
/// "/inbox" — the inbox must stay under the cache dir it was derived from.
fn is_under(path: &Path, base: &Path) -> bool {
    path.components().count() >= base.components().count()
        && path
            .components()
            .zip(base.components())
            .all(|(a, b)| a == b)
}

/// Where a saved file goes. Pure (base/date supplied) so it can be tested:
/// every component is sanitised and the result is always under `base`.
/// (Used by tests; `write_inbox_file` takes the already-dated dir.)
#[allow(dead_code)]
pub fn inbox_path_under(base: &Path, date: &str, message_id: i64, ext: &str) -> PathBuf {
    let dates: String = date.chars().filter(|c| c.is_ascii_digit()).collect();
    let date = if dates.len() == 8 { dates } else { "19700101".to_string() };
    let ext = sanitise_ext(ext).unwrap_or_else(|| "bin".to_string());
    let path = base.join(&date).join(format!("{message_id}.{ext}"));
    debug_assert!(is_under(&path, base));
    path
}

/// The inbox root: under the OS cache dir, never attacker-chosen.
pub fn inbox_dir() -> PathBuf {
    let base = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("telepager").join("inbox")
}

/// The current day as `YYYYMMDD` (UTC) for the dated inbox subdirectory.
/// No date crate, so the calendar conversion is done by hand.
pub fn yyyymmdd_from_unix(secs: u64) -> String {
    // Howard Hinnant's civil_from_days, days since 1970-01-01
    let z = (secs / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}{m:02}{d:02}")
}

/// Write downloaded bytes into `dir` atomically (tmp, then rename) and refuse
/// oversize files before they land. The extension is sanitised, so the final
/// path is always directly under `dir`.
pub fn write_inbox_file(dir: &Path, message_id: i64, ext: &str, bytes: &[u8]) -> Result<PathBuf> {
    check_file_size(bytes.len())?;
    let ext = sanitise_ext(ext).unwrap_or_else(|| "bin".to_string());
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let final_path = dir.join(format!("{message_id}.{ext}"));
    debug_assert!(is_under(&final_path, dir));
    let tmp_path = dir.join(format!("{message_id}.{ext}.tmp"));
    // a symlink at either end would turn the tmp write or the rename into a
    // write / replace of something outside the inbox
    for path in [&tmp_path, &final_path] {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                bail!("refusing to follow symlink {}", path.display());
            }
        }
    }
    // saved files can carry anything, so they are owner-only from creation
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp_path).with_context(|| format!("writing {}", tmp_path.display()))?;
    file.write_all(bytes).with_context(|| format!("writing {}", tmp_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("writing {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), final_path.display()))?;
    Ok(final_path)
}

/// The url a downloaded file is served from. Prod turns the api base
/// `https://api.telegram.org/bot<token>` into `.../file/bot<token>/<path>`;
/// a local fake base (tests) is used verbatim.
pub fn file_url_for_base(base: &str, file_path: &str) -> String {
    let base = base.trim_end_matches('/');
    let file_path = file_path.trim_start_matches('/');
    let file_base = if base.contains("/bot") {
        base.replacen("/bot", "/file/bot", 1)
    } else {
        base.to_string()
    };
    format!("{file_base}/{file_path}")
}

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

        let cut = rest
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());

        // back up to the last newline so lines don't get cut in half, unless it's miles away
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
    fn an_oversize_declared_file_size_is_refused_before_downloading() {
        assert!(check_declared_size(Some(MAX_FILE_SIZE as u64 + 1)).is_err());
        assert!(check_declared_size(Some(1024)).is_ok());
        assert!(check_declared_size(None).is_ok());
    }

    #[test]
    fn an_oversize_content_length_is_refused_before_buffering() {
        assert!(check_content_length(Some(MAX_FILE_SIZE as u64 + 1)).is_err());
        assert!(check_content_length(Some(1024)).is_ok());
        assert!(check_content_length(None).is_ok());
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
    fn unmodified_edits_are_recognised() {
        let e = anyhow::anyhow!(
            "telegram rejected editMessageText: Bad Request: message is not modified: ..."
        );
        assert!(is_unmodified(&e));
        assert!(!is_unmodified(&anyhow::anyhow!("telegram rejected editMessageText: Bad Request: chat not found")));
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("héllo", 3), "hél");
        assert_eq!(truncate("hi", 10), "hi");
    }

    // ---- Feature 3: Telegram media inbound (tests written first) ----

    #[test]
    fn photo_array_picks_the_largest_variant() {
        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":42,"chat":{"id":7},"from":{"id":7},"photo":[{"file_id":"small","width":90,"height":90},{"file_id":"big","width":1280,"height":960}],"caption":"look at this"}}"#,
        )
        .unwrap();
        let msg = upd.message.unwrap();
        assert_eq!(msg.largest_photo().unwrap().file_id, "big");
        assert_eq!(msg.caption.as_deref(), Some("look at this"));
    }

    #[test]
    fn voice_parses_with_duration_and_mime() {
        let upd: Update = serde_json::from_str(
            r#"{"update_id":2,"message":{"message_id":43,"chat":{"id":7},"from":{"id":7},"voice":{"file_id":"v1","mime_type":"audio/ogg","duration":13},"caption":"listen"}}"#,
        )
        .unwrap();
        let msg = upd.message.unwrap();
        let voice = msg.voice.as_ref().unwrap();
        assert_eq!(voice.file_id, "v1");
        assert_eq!(voice.mime_type.as_deref(), Some("audio/ogg"));
        assert_eq!(voice.duration, Some(13));
        assert_eq!(msg.caption.as_deref(), Some("listen"));
    }

    #[test]
    fn document_parses_with_file_name_and_caption() {
        let upd: Update = serde_json::from_str(
            r#"{"update_id":3,"message":{"message_id":44,"chat":{"id":7},"from":{"id":7},"document":{"file_id":"d1","file_name":"notes.txt","mime_type":"text/plain"},"caption":"here you go"}}"#,
        )
        .unwrap();
        let msg = upd.message.unwrap();
        let doc = msg.document.as_ref().unwrap();
        assert_eq!(doc.file_id, "d1");
        assert_eq!(doc.file_name.as_deref(), Some("notes.txt"));
        assert_eq!(doc.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(msg.caption.as_deref(), Some("here you go"));
    }

    #[test]
    fn media_plan_covers_photo_voice_document_audio_and_video_note() {
        let photo: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"photo":[{"file_id":"s","width":1,"height":1},{"file_id":"L","width":9,"height":9}]}}"#,
        )
        .unwrap();
        let plan = media_plan(photo.message.as_ref().unwrap()).unwrap();
        assert_eq!(plan.file_id, "L");
        assert_eq!(plan.kind, "photo");

        let voice: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"voice":{"file_id":"v","duration":5}}}"#,
        )
        .unwrap();
        assert_eq!(media_plan(voice.message.as_ref().unwrap()).unwrap().kind, "voice");

        let audio: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"audio":{"file_id":"a","duration":5}}}"#,
        )
        .unwrap();
        assert_eq!(media_plan(audio.message.as_ref().unwrap()).unwrap().kind, "audio");

        let note: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"video_note":{"file_id":"n","duration":3}}}"#,
        )
        .unwrap();
        assert_eq!(media_plan(note.message.as_ref().unwrap()).unwrap().kind, "video_note");

        let doc: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"document":{"file_id":"d","mime_type":"application/pdf"}}}"#,
        )
        .unwrap();
        let plan = media_plan(doc.message.as_ref().unwrap()).unwrap();
        assert_eq!(plan.kind, "document");
        assert_eq!(plan.ext, "pdf");
    }

    #[test]
    fn plain_text_has_no_media_plan() {
        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"text":"hello"}}"#,
        )
        .unwrap();
        assert!(media_plan(upd.message.as_ref().unwrap()).is_none());
    }

    #[test]
    fn caption_without_media_still_has_no_plan() {
        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"text":"hello","caption":"cap"}}"#,
        )
        .unwrap();
        assert!(media_plan(upd.message.as_ref().unwrap()).is_none());
    }

    #[test]
    fn evil_file_names_cannot_escape_the_inbox() {
        // traversal in the telegram-supplied name: only a whitelisted extension survives
        assert_eq!(ext_for_mime(None, Some("../../etc/passwd"), "bin"), "bin");
        assert_eq!(ext_for_mime(None, Some("..\\..\\evil.png"), "bin"), "png");
        assert_eq!(ext_for_mime(Some("image/jpeg"), Some("../../../x.jpg"), "bin"), "jpeg");
        // a poisoned mime type cannot smuggle a separator into the extension
        // (params are stripped, and anything that is not a known mime falls back)
        assert_eq!(ext_for_mime(Some("text/plain; ../../x"), None, "bin"), "txt");
        assert_eq!(ext_for_mime(Some("text/..\\evil"), None, "bin"), "bin");
        assert_eq!(ext_for_mime(Some("image/png"), None, "bin"), "png");
        assert_eq!(ext_for_mime(Some("audio/ogg"), None, "bin"), "ogg");
        assert_eq!(ext_for_mime(Some("application/pdf"), Some("a.exe"), "bin"), "pdf");
        // the path itself is always under the inbox base
        let base = std::path::Path::new("/tmp/inbox");
        let p = inbox_path_under(base, "20261108", 99, "png");
        assert!(p.starts_with(base));
        assert!(!p.to_string_lossy().contains(".."));
        assert_eq!(p.file_name().unwrap(), "99.png");
        // even a hostile ext argument is sanitised before it lands in a path
        let hostile = inbox_path_under(base, "20261108", 1, "../../etc/passwd");
        assert!(hostile.starts_with(base));
        assert!(!hostile.to_string_lossy().contains(".."));
    }

    #[test]
    fn oversize_files_are_rejected_before_saving() {
        assert!(check_file_size(0).is_ok());
        assert!(check_file_size(MAX_FILE_SIZE).is_ok());
        assert!(check_file_size(MAX_FILE_SIZE + 1).is_err());
    }

    #[test]
    fn media_prompt_carries_path_caption_and_voice_details() {
        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":7,"voice":{"file_id":"v","mime_type":"audio/ogg","duration":42},"caption":"play it"}}"#,
        )
        .unwrap();
        let plan = media_plan(upd.message.as_ref().unwrap()).unwrap();
        let prompt = media_prompt(&plan, std::path::Path::new("/tmp/inbox/20261108/7.ogg"));
        assert!(prompt.contains("/tmp/inbox/20261108/7.ogg"), "{prompt}");
        assert!(prompt.contains("play it"), "{prompt}");
        assert!(prompt.contains("42"), "{prompt}");
        assert!(prompt.contains("audio/ogg"), "{prompt}");
        assert!(prompt.contains("STT"), "{prompt}");

        let photo: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":8,"photo":[{"file_id":"p","width":1,"height":1}]}}"#,
        )
        .unwrap();
        let plan = media_plan(photo.message.as_ref().unwrap()).unwrap();
        let prompt = media_prompt(&plan, std::path::Path::new("/tmp/inbox/20261108/8.jpg"));
        assert!(prompt.contains("photo"), "{prompt}");
        assert!(prompt.contains("8.jpg"), "{prompt}");
    }

    #[test]
    fn yyyymmdd_follows_the_calendar() {
        assert_eq!(yyyymmdd_from_unix(0), "19700101");
        // 2024-01-01 00:00 UTC
        assert_eq!(yyyymmdd_from_unix(1704067200), "20240101");
        // 2026-11-08 00:00 UTC
        assert_eq!(yyyymmdd_from_unix(1794096000), "20261108");
    }

    #[test]
    fn file_urls_keep_prod_and_test_bases_apart() {
        // prod: https://api.telegram.org/bot<token> -> .../file/bot<token>/<path>
        let prod = file_url_for_base("https://api.telegram.org/bot123:SECRET", "photos/file_1.jpg");
        assert_eq!(prod, "https://api.telegram.org/file/bot123:SECRET/photos/file_1.jpg");
        // test: a local fake base is used verbatim
        let test = file_url_for_base("http://127.0.0.1:9999", "photos/file_1.jpg");
        assert_eq!(test, "http://127.0.0.1:9999/photos/file_1.jpg");
    }

    #[test]
    fn inbox_dir_is_under_the_cache_dir() {
        let dir = inbox_dir();
        let cache = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
        assert!(dir.starts_with(&cache), "{} not under cache dir", dir.display());
        assert!(dir.ends_with("telepager/inbox") || dir.to_string_lossy().ends_with("telepager/inbox"));
    }

    #[cfg(unix)]
    #[test]
    fn the_inbox_refuses_to_follow_a_planted_symlink() {
        let dir = std::env::temp_dir().join(format!("telepager-inbox-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        let tmp = dir.join("123.png.tmp");
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();

        let err = write_inbox_file(&dir, 123, "png", b"hi").map(|_| ()).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        // following the link would have overwritten the victim
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
        assert!(!dir.join("123.png").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_inbox_file_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("telepager-inbox-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saved = write_inbox_file(&dir, 55, "png", b"hello").unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chat_type_distinguishes_private_from_group() {
        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"chat":{"id":-100,"type":"supergroup"},"from":{"id":7},"text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(upd.message.unwrap().chat.unwrap().typ.as_deref(), Some("supergroup"));

        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"chat":{"id":7,"type":"private"},"from":{"id":7},"text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(upd.message.unwrap().chat.unwrap().typ.as_deref(), Some("private"));

        // chats telegram doesn't annotate (old payloads) read as unknown
        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"chat":{"id":7},"from":{"id":7},"text":"hi"}}"#,
        )
        .unwrap();
        assert!(upd.message.unwrap().chat.unwrap().typ.is_none());
    }

    #[test]
    fn writing_to_the_inbox_uses_tmp_then_rename() {
        let dir = std::env::temp_dir().join(format!("telepager-inbox-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saved = write_inbox_file(&dir, 123, "png", b"hello").unwrap();
        assert_eq!(saved, dir.join("123.png"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"hello");
        // no tmp litter left behind
        assert!(!dir.join("123.png.tmp").exists());
        // oversize bytes never land on disk
        let big = vec![0u8; MAX_FILE_SIZE + 1];
        assert!(write_inbox_file(&dir, 124, "bin", &big).is_err());
        assert!(!dir.join("124.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn message_dates_decide_staleness_at_120_seconds() {
        let now = 1_700_000_000i64;
        assert!(is_stale(Some(now - 121), now));
        assert!(is_stale(Some(now - 3600), now));
        assert!(!is_stale(Some(now - 120), now));
        assert!(!is_stale(Some(now), now));
        assert!(!is_stale(None, now));
        assert!(!is_stale(Some(0), now));
        // parsed from a real payload
        let upd: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":1,"date":1700000000,"text":"/sh echo hi"}}"#,
        )
        .unwrap();
        let msg = upd.message.unwrap();
        assert_eq!(msg.date, Some(1700000000));
        assert!(msg.is_stale(1700000000 + 121));
        assert!(!msg.is_stale(1700000000 + 60));
    }

    #[test]
    fn percent_encoded_tokens_are_scrubbed_too() {
        let tg = Telegram::new("123456:SECRET-TOKEN").unwrap();
        assert_eq!(percent_encode("123456:SECRET-TOKEN"), "123456%3ASECRET%2DTOKEN");
        let leaky = anyhow::anyhow!("calling https://api.telegram.org/bot123456%3ASECRET%2DTOKEN/send");
        let scrubbed = format!("{:#}", tg.scrub(leaky));
        assert!(!scrubbed.contains("SECRET"), "{scrubbed}");
        assert!(scrubbed.contains("<redacted>"), "{scrubbed}");
    }
}
