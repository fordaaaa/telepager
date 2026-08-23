use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    bot_token: Option<String>,
    allowed_user_ids: Option<Vec<i64>>,
    chat_id: Option<i64>,
    ask_timeout_seconds: Option<u64>,
    master: Option<MasterConfig>,
    agents: Option<BTreeMap<String, AgentPreset>>,
    allowed_dirs: Option<Vec<PathBuf>>,
    ui_port: Option<u16>,
}

/// Which backend the master agent runs on.
///
/// Most are HTTP APIs, where the wire formats differ enough to be worth naming
/// — though everything OpenAI-shaped (openrouter, groq, together, lm studio,
/// vllm, ollama's /v1) is one variant with a base_url.
///
/// The last two are different in kind: they drive a coding CLI that is already
/// logged in, so they need no API key at all. `claude-code` uses your Claude
/// Code subscription, `opencode` whatever opencode is configured with —
/// including its free models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Anthropic,
    #[serde(alias = "openai-compatible", alias = "openrouter", alias = "groq", alias = "together")]
    Openai,
    #[serde(alias = "google")]
    Gemini,
    Ollama,
    #[serde(alias = "claude", alias = "claudecode")]
    ClaudeCode,
    #[serde(alias = "opencode-cli")]
    Opencode,
}

impl Provider {
    /// The env var people already have exported for this provider.
    pub fn default_key_env(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Openai => "OPENAI_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::Ollama => "OLLAMA_API_KEY",
            // these log in through their own CLI, not an env var
            Provider::ClaudeCode | Provider::Opencode => "",
        }
    }

    /// Every other env var worth checking before giving up on a key. Lets
    /// someone with only OPENROUTER_API_KEY set get a working master agent
    /// without writing any config at all.
    pub fn fallback_key_envs(self) -> &'static [&'static str] {
        match self {
            Provider::Anthropic => &["CLAUDE_API_KEY"],
            Provider::Openai => &["OPENROUTER_API_KEY", "GROQ_API_KEY", "TOGETHER_API_KEY"],
            Provider::Gemini => &["GOOGLE_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"],
            Provider::Ollama | Provider::ClaudeCode | Provider::Opencode => &[],
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::Openai => "https://api.openai.com/v1",
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Provider::Ollama => "http://127.0.0.1:11434/v1",
            Provider::ClaudeCode | Provider::Opencode => "",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::Anthropic => "claude-sonnet-4-5-20250929",
            Provider::Openai => "gpt-4o",
            Provider::Gemini => "gemini-2.0-flash",
            Provider::Ollama => "llama3.1",
            // empty means "whatever the cli is already set to"
            Provider::ClaudeCode | Provider::Opencode => "",
        }
    }

    /// Whether this backend is a coding CLI rather than an HTTP API.
    pub fn is_cli(self) -> bool {
        matches!(self, Provider::ClaudeCode | Provider::Opencode)
    }

    /// The command a CLI backend runs.
    pub fn cli_command(self) -> Option<&'static str> {
        match self {
            Provider::ClaudeCode => Some("claude"),
            Provider::Opencode => Some("opencode"),
            _ => None,
        }
    }

    /// Ollama runs on your machine and the CLI backends carry their own login,
    /// so a missing key is normal for all three.
    pub fn needs_key(self) -> bool {
        !matches!(self, Provider::Ollama | Provider::ClaudeCode | Provider::Opencode)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
            Provider::Gemini => "gemini",
            Provider::Ollama => "ollama",
            Provider::ClaudeCode => "claude-code",
            Provider::Opencode => "opencode",
        }
    }

    /// How the console describes this backend in the picker.
    pub fn blurb(self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic API",
            Provider::Openai => "OpenAI, or anything OpenAI-shaped",
            Provider::Gemini => "Google Gemini API",
            Provider::Ollama => "Ollama, running locally",
            Provider::ClaudeCode => "your Claude Code login — no API key",
            Provider::Opencode => "opencode's models, free ones included — no API key",
        }
    }

    pub fn all() -> &'static [Provider] {
        &[
            Provider::ClaudeCode,
            Provider::Opencode,
            Provider::Anthropic,
            Provider::Openai,
            Provider::Gemini,
            Provider::Ollama,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MasterConfig {
    pub provider: Provider,
    pub model: String,
    /// An inline key. Optional, and the env var is the better habit.
    pub api_key: Option<String>,
    /// Read the key from this env var instead of the provider default.
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    /// Appended to the built-in system prompt rather than replacing it, so
    /// personal instructions can't accidentally delete the tool contract.
    pub system: Option<String>,
    pub max_tokens: u32,
    /// Override the binary a CLI backend runs, if it isn't on PATH under the
    /// usual name.
    pub command: Option<String>,
    /// Extra arguments appended to a CLI backend's command line.
    pub args: Vec<String>,
}

impl Default for MasterConfig {
    fn default() -> Self {
        Self {
            // the one that needs no api key, and that most people running a
            // coding agent already have installed
            provider: Provider::ClaudeCode,
            model: String::new(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            system: None,
            max_tokens: 4096,
            command: None,
            args: Vec::new(),
        }
    }
}

impl MasterConfig {
    pub fn model_or_default(&self) -> String {
        if self.model.trim().is_empty() {
            self.provider.default_model().to_string()
        } else {
            self.model.clone()
        }
    }

    pub fn base_url_or_default(&self) -> String {
        self.base_url
            .as_deref()
            .map(|s| s.trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.provider.default_base_url().to_string())
    }

    /// The key, from config or from whichever env var is actually set.
    pub fn resolve_key(&self) -> Option<String> {
        if let Some(k) = self.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            return Some(k.to_string());
        }
        let mut names: Vec<&str> = Vec::new();
        if let Some(n) = self.api_key_env.as_deref().filter(|n| !n.trim().is_empty()) {
            names.push(n);
        }
        names.push(self.provider.default_key_env());
        names.extend_from_slice(self.provider.fallback_key_envs());

        names.into_iter().find_map(|name| {
            std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        })
    }

    /// The command a CLI backend runs, honouring an override in the config.
    pub fn cli_command(&self) -> Option<String> {
        let explicit = self.command.as_deref().map(str::trim).filter(|c| !c.is_empty());
        match (explicit, self.provider.cli_command()) {
            (Some(c), _) => Some(c.to_string()),
            (None, Some(c)) => Some(c.to_string()),
            (None, None) => None,
        }
    }

    pub fn is_usable(&self) -> bool {
        if self.provider.is_cli() {
            // logged in is the cli's problem; installed is ours
            return self
                .cli_command()
                .is_some_and(|c| crate::agents::which(&c).is_some());
        }
        !self.provider.needs_key() || self.resolve_key().is_some()
    }
}

/// A worker agent: some coding CLI we can spawn in a directory with a task.
/// `{task}` in an arg is replaced with the task text; an arg that is exactly
/// `{task}` and nothing else is replaced as a whole argument, so tasks with
/// spaces and quotes survive without a shell.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPreset {
    pub command: String,
    pub args: Vec<String>,
    /// Shown in the UI picker.
    pub description: String,
    /// Extra env for the child, e.g. a per-agent API key.
    pub env: BTreeMap<String, String>,
    /// Whether this agent keeps reading stdin after it starts, which decides
    /// if "send a follow-up" is offered for its sessions.
    pub interactive: bool,
}

/// The coding CLIs telepager knows how to drive out of the box. Anything on
/// PATH shows up in the UI with no configuration; anything else can be added
/// under `agents` in the config file.
pub fn builtin_agents() -> BTreeMap<String, AgentPreset> {
    let mut m = BTreeMap::new();
    let mut add = |name: &str, command: &str, args: &[&str], description: &str| {
        m.insert(
            name.to_string(),
            AgentPreset {
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                description: description.to_string(),
                env: BTreeMap::new(),
                interactive: false,
            },
        );
    };

    add("claude", "claude", &["-p", "{task}", "--permission-mode", "acceptEdits"],
        "Claude Code, headless");
    add("codex", "codex", &["exec", "--full-auto", "{task}"],
        "OpenAI Codex CLI");
    add("gemini", "gemini", &["-y", "-p", "{task}"],
        "Google Gemini CLI");
    add("opencode", "opencode", &["run", "{task}"],
        "opencode, any model it's configured with");
    add("cursor", "cursor-agent", &["-p", "{task}", "--force"],
        "Cursor CLI agent");
    add("amp", "amp", &["-x", "{task}"],
        "Sourcegraph Amp");
    add("aider", "aider", &["--yes", "--no-check-update", "--message", "{task}"],
        "aider, any model it's configured with");
    add("crush", "crush", &["run", "{task}"],
        "Charm Crush");
    add("qwen", "qwen", &["-y", "-p", "{task}"],
        "Qwen Code CLI");
    add("goose", "goose", &["run", "-t", "{task}"],
        "Block Goose");
    m
}

#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    pub allowed_user_ids: Vec<i64>,
    pub chat_id: i64,
    pub ask_timeout_seconds: u64,
    pub master: MasterConfig,
    pub agents: BTreeMap<String, AgentPreset>,
    /// Directories Telegram is allowed to spawn agents in. Empty means
    /// Telegram cannot spawn anything; the local web UI is not restricted,
    /// since reaching it already means being at the machine.
    pub allowed_dirs: Vec<PathBuf>,
    pub ui_port: u16,
}

impl Config {
    /// Every chat we page, primary one first. `chat_id` stays the default so
    /// old config files keep working; the rest of the allowlist gets the same
    /// messages, which is the point of allowing them.
    pub fn chat_ids(&self) -> Vec<i64> {
        let mut ids = vec![self.chat_id];
        for id in &self.allowed_user_ids {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        ids
    }
}

// %APPDATA%\telepager on windows, ~/Library/Application Support on mac,
// $XDG_CONFIG_HOME or ~/.config on linux. ~/.config stays as a fallback
// everywhere unix since that's what the readme has always said.
fn config_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Some(dir) = dirs::config_dir() {
        out.push(dir.join("telepager").join("config.json"));
    }

    if !cfg!(windows) {
        if let Some(home) = dirs::home_dir() {
            let p = home.join(".config").join("telepager").join("config.json");
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }

    out
}

pub fn first_candidate() -> String {
    config_candidates()
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "telepager.config.json".into())
}

fn resolve_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }

    let cwd = PathBuf::from("telepager.config.json");
    if cwd.exists() {
        return Some(cwd);
    }

    config_candidates().into_iter().find(|p| p.exists())
}

// the config file we'd read right now, if there is one. an explicit --config
// that doesn't exist yet is a file we're about to write, not one we have.
pub fn existing_path(explicit: Option<&Path>) -> Option<PathBuf> {
    resolve_config_path(explicit).filter(|p| p.exists())
}

// where a fresh config should be written
pub fn target_path(explicit: Option<&Path>) -> PathBuf {
    resolve_config_path(explicit).unwrap_or_else(|| {
        config_candidates()
            .into_iter()
            .next()
            .unwrap_or_else(|| PathBuf::from("telepager.config.json"))
    })
}

/// Read the config file as raw JSON, so a caller can edit one key and write
/// the rest back untouched.
fn read_doc(path: &Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(_) => Ok(serde_json::json!({})),
    }
}

fn write_doc(path: &Path, doc: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(doc)?)
        .with_context(|| format!("writing {}", path.display()))?;

    // the token is a credential, so don't leave it world readable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// write bot_token and allowed_user_ids without stomping on anything else
// the file already had
pub fn save(explicit: Option<&Path>, token: &str, ids: &[i64]) -> Result<PathBuf> {
    let path = target_path(explicit);
    let mut doc = read_doc(&path)?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;

    obj.insert("bot_token".into(), serde_json::json!(token));
    obj.insert("allowed_user_ids".into(), serde_json::json!(ids));

    write_doc(&path, &doc)?;
    Ok(path)
}

/// Replace just the `master` block, leaving the rest of the file alone.
pub fn save_master(explicit: Option<&Path>, master: &MasterConfig) -> Result<PathBuf> {
    let path = target_path(explicit);
    let mut doc = read_doc(&path)?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;

    let mut value = serde_json::to_value(master)?;
    // an empty inline key is noise in the file, and null reads as "unset"
    if let Some(o) = value.as_object_mut() {
        o.retain(|_, v| !v.is_null());
        if master.api_key.as_deref().map(str::trim).unwrap_or("").is_empty() {
            o.remove("api_key");
        }
    }
    obj.insert("master".into(), value);

    write_doc(&path, &doc)?;
    Ok(path)
}

/// Replace the directories Telegram may spawn agents in.
pub fn save_allowed_dirs(explicit: Option<&Path>, dirs: &[PathBuf]) -> Result<PathBuf> {
    let path = target_path(explicit);
    let mut doc = read_doc(&path)?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    obj.insert("allowed_dirs".into(), serde_json::json!(dirs));
    write_doc(&path, &doc)?;
    Ok(path)
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let file = resolve_config_path(explicit);

    let raw: RawConfig = match &file {
        Some(path) if path.exists() => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            if text.trim().is_empty() {
                RawConfig::default()
            } else {
                serde_json::from_str(&text)
                    .with_context(|| format!("{} is not valid JSON", path.display()))?
            }
        }
        _ => RawConfig::default(),
    };

    let token = match std::env::var("TELEGRAM_BOT_TOKEN") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => raw.bot_token.clone().unwrap_or_default(),
    };
    if token.trim().is_empty() {
        let where_to_put_it = first_candidate();
        bail!(
            "No bot token found. Set TELEGRAM_BOT_TOKEN or put bot_token in the \
             config file ({where_to_put_it})."
        );
    }

    let allowed_user_ids = match std::env::var("TELEGRAM_ALLOWED_IDS") {
        Ok(v) if !v.trim().is_empty() => parse_allowed_ids(&v)?,
        _ => raw.allowed_user_ids.clone().unwrap_or_default(),
    };
    if allowed_user_ids.is_empty() {
        bail!(
            "allowed_user_ids is empty. This is the whole security model — refusing \
             to start with an empty allowlist. Set TELEGRAM_ALLOWED_IDS or add \
             allowed_user_ids to your config file."
        );
    }

    // in a private chat the chat id is just the user id
    let chat_id = raw
        .chat_id
        .unwrap_or_else(|| *allowed_user_ids.iter().min().unwrap());

    // configured agents override a builtin of the same name, and add new ones
    let mut agents = builtin_agents();
    for (name, preset) in raw.agents.unwrap_or_default() {
        agents.insert(name, preset);
    }

    Ok(Config {
        token,
        allowed_user_ids,
        chat_id,
        ask_timeout_seconds: raw.ask_timeout_seconds.unwrap_or(300),
        master: raw.master.unwrap_or_default(),
        agents,
        allowed_dirs: raw.allowed_dirs.unwrap_or_default(),
        ui_port: raw.ui_port.unwrap_or(0),
    })
}

/// Like `load`, but a missing or half-finished config is `None` rather than an
/// error. The app has to be able to start unconfigured — that's the whole
/// point of the setup page being served by the running app.
pub fn load_optional(explicit: Option<&Path>) -> Option<Config> {
    load(explicit).ok()
}

fn parse_allowed_ids(s: &str) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let id = part
            .parse::<i64>()
            .with_context(|| format!("TELEGRAM_ALLOWED_IDS: '{part}' is not an integer"))?;
        ids.push(id);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ids_ok() {
        assert_eq!(parse_allowed_ids("1, 2,3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_allowed_ids("-5,10").unwrap(), vec![-5, 10]);
    }

    #[test]
    fn parse_ids_skips_blanks() {
        assert_eq!(parse_allowed_ids("1,,2, ,3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parse_ids_rejects_non_integer() {
        assert!(parse_allowed_ids("1,x,2").is_err());
    }

    #[test]
    fn candidates_point_at_telepager() {
        let c = config_candidates();
        assert!(!c.is_empty());
        assert!(c.iter().all(|p| p.ends_with("telepager/config.json")));
    }

    #[cfg(not(windows))]
    #[test]
    fn dot_config_is_a_candidate_on_unix() {
        let c = config_candidates();
        assert!(c.iter().any(|p| p.to_string_lossy().contains(".config/telepager")));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("telepager-{name}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn everyone_allowed_gets_paged_with_the_primary_chat_first() {
        let cfg = Config {
            token: "t".into(),
            allowed_user_ids: vec![7, 8, 9],
            chat_id: 8,
            ask_timeout_seconds: 300,
            master: Default::default(),
            agents: Default::default(),
            allowed_dirs: Vec::new(),
            ui_port: 0,
        };
        assert_eq!(cfg.chat_ids(), vec![8, 7, 9]);

        // a group chat isn't in the allowlist, and still gets everything
        let group = Config { chat_id: -100, ..cfg };
        assert_eq!(group.chat_ids(), vec![-100, 7, 8, 9]);
    }

    #[test]
    fn save_keeps_other_keys() {
        let dir = scratch("cfg");
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"ask_timeout_seconds": 900, "bot_token": "old"}"#).unwrap();

        save(Some(&path), "new", &[7, 8]).unwrap();

        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["bot_token"], "new");
        assert_eq!(back["allowed_user_ids"], serde_json::json!([7, 8]));
        assert_eq!(back["ask_timeout_seconds"], 900);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_creates_a_missing_file() {
        let dir = scratch("new");
        let path = dir.join("nested").join("config.json");

        save(Some(&path), "tok", &[1]).unwrap();

        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["bot_token"], "tok");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_master_leaves_the_bot_token_alone() {
        let dir = scratch("master");
        let path = dir.join("config.json");
        save(Some(&path), "tok", &[1]).unwrap();

        let master = MasterConfig {
            provider: Provider::Openai,
            model: "gpt-4o-mini".into(),
            ..MasterConfig::default()
        };
        save_master(Some(&path), &master).unwrap();

        let cfg = load(Some(&path)).unwrap();
        assert_eq!(cfg.token, "tok");
        assert_eq!(cfg.master.provider, Provider::Openai);
        assert_eq!(cfg.master.model_or_default(), "gpt-4o-mini");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_inline_key_is_not_written() {
        let dir = scratch("nokey");
        let path = dir.join("config.json");
        let master = MasterConfig { api_key: Some("   ".into()), ..MasterConfig::default() };
        save_master(Some(&path), &master).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("api_key"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn configured_agents_override_builtins_and_add_new_ones() {
        let dir = scratch("agents");
        let path = dir.join("config.json");
        std::fs::write(&path, serde_json::json!({
            "bot_token": "t",
            "allowed_user_ids": [1],
            "agents": {
                "claude": { "command": "my-claude", "args": ["{task}"] },
                "mine":   { "command": "mine", "args": [] }
            }
        }).to_string()).unwrap();

        let cfg = load(Some(&path)).unwrap();
        assert_eq!(cfg.agents["claude"].command, "my-claude");
        assert_eq!(cfg.agents["mine"].command, "mine");
        // untouched builtins survive
        assert!(cfg.agents.contains_key("codex"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_config_is_none_not_an_error() {
        assert!(load_optional(Some(Path::new("/nope/telepager/does-not-exist.json"))).is_none());
    }

    #[test]
    fn model_and_base_url_fall_back_to_the_provider_defaults() {
        let m = MasterConfig { provider: Provider::Gemini, ..MasterConfig::default() };
        assert_eq!(m.model_or_default(), "gemini-2.0-flash");
        assert!(m.base_url_or_default().contains("generativelanguage"));

        let custom = MasterConfig {
            provider: Provider::Openai,
            base_url: Some("https://openrouter.ai/api/v1/".into()),
            ..MasterConfig::default()
        };
        // the trailing slash would double up when paths get appended
        assert_eq!(custom.base_url_or_default(), "https://openrouter.ai/api/v1");
    }

    #[test]
    fn provider_names_from_config_cover_the_common_aliases() {
        let p: Provider = serde_json::from_str(r#""openrouter""#).unwrap();
        assert_eq!(p, Provider::Openai);
        let p: Provider = serde_json::from_str(r#""google""#).unwrap();
        assert_eq!(p, Provider::Gemini);
        let p: Provider = serde_json::from_str(r#""openai-compatible""#).unwrap();
        assert_eq!(p, Provider::Openai);
    }

    #[test]
    fn the_default_master_needs_no_api_key() {
        let m = MasterConfig::default();
        assert_eq!(m.provider, Provider::ClaudeCode);
        assert!(!m.provider.needs_key());
        assert!(m.provider.is_cli());
        assert_eq!(m.cli_command().as_deref(), Some("claude"));
    }

    #[test]
    fn a_cli_master_is_usable_only_when_its_command_exists() {
        let missing = MasterConfig {
            provider: Provider::ClaudeCode,
            command: Some("telepager-not-a-real-cli".into()),
            ..MasterConfig::default()
        };
        assert!(!missing.is_usable());

        let present = MasterConfig {
            provider: Provider::ClaudeCode,
            command: Some("sh".into()),
            ..MasterConfig::default()
        };
        assert_eq!(present.is_usable(), crate::agents::which("sh").is_some());
    }

    #[test]
    fn ollama_is_usable_without_a_key() {
        let m = MasterConfig { provider: Provider::Ollama, ..MasterConfig::default() };
        assert!(m.is_usable());
    }
}
