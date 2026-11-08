use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The console's port when nothing in config pins one. Fixed rather than
/// random so the console sits at a stable address you can bookmark; if it's
/// taken, `web::start` falls back to a free one instead of failing.
pub const DEFAULT_UI_PORT: u16 = 47823;

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
    permissions: Option<Permissions>,
}

/// What telepager may do beyond starting worker agents. Everything but the
/// confirmation prompt is off until someone ticks a box, upgrades included.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    /// May the master agent run shell commands at all.
    pub shell: bool,
    /// May settings be changed from Telegram.
    pub remote_control: bool,
    /// Ask before running anything that looks destructive.
    pub confirm_destructive: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self { shell: false, remote_control: false, confirm_destructive: true }
    }
}

/// The two shapes `telepager init` can write: a pager that can't run anything,
/// or the full local console with shell and remote control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InitPreset {
    #[default]
    PagerOnly,
    Full,
}

impl InitPreset {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pager-only" => Some(Self::PagerOnly),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn permissions(self) -> Permissions {
        match self {
            Self::PagerOnly => Permissions {
                shell: false,
                remote_control: false,
                confirm_destructive: true,
            },
            Self::Full => Permissions {
                shell: true,
                remote_control: true,
                confirm_destructive: true,
            },
        }
    }
}

/// Which backend the master agent runs on. Most are HTTP APIs whose wire
/// formats differ enough to name, though everything openai-shaped (openrouter,
/// groq, together, lm studio, vllm, ollama's /v1) is one variant with a
/// base_url.
///
/// The last two are different in kind: they drive an already logged-in coding
/// CLI, so they need no api key at all.
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

    /// Other env vars worth checking before giving up on a key, so someone with
    /// just OPENROUTER_API_KEY set gets a working master with no config.
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

    /// A provider, or one of the services that speak another's API from a
    /// different address. Naming one of those picks the provider and the
    /// address together, which is the part that's easy to get wrong by hand.
    pub fn named(name: &str) -> Option<(Provider, Option<&'static str>)> {
        let name = name.trim().to_lowercase();
        let base = match name.as_str() {
            "openrouter" => "https://openrouter.ai/api/v1",
            "groq" => "https://api.groq.com/openai/v1",
            "together" => "https://api.together.xyz/v1",
            _ => {
                let provider = serde_json::from_value(serde_json::json!(name)).ok()?;
                return Some((provider, None));
            }
        };
        Some((Provider::Openai, Some(base)))
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

    /// Ollama is local and the CLI backends carry their own login, so a missing
    /// key is normal for those.
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

    /// Whether `name` is an allowed api-key env var for this provider: the
    /// provider's own default plus its fallbacks, nothing else. In particular
    /// TELEGRAM_BOT_TOKEN (and any other unrelated var) is never allowed —
    /// a `/key env` or console `api_key_env` naming it would exfiltrate the
    /// bot token as a model key.
    pub fn allows_key_env(self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let default = self.default_key_env();
        (!default.is_empty() && default == name) || self.fallback_key_envs().contains(&name)
    }

    /// Validate an `api_key_env` choice for this provider.
    pub fn validate_api_key_env(self, name: &str) -> Result<()> {
        let name = name.trim();
        if self.allows_key_env(name) {
            Ok(())
        } else {
            bail!("refusing to use {name} as api key")
        }
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
    /// Appended to the built-in system prompt, not replacing it, so personal
    /// instructions can't delete the tool contract.
    pub system: Option<String>,
    pub max_tokens: u32,
    /// Override the binary a CLI backend runs, if it's not on PATH under the
    /// usual name.
    pub command: Option<String>,
    /// Extra arguments appended to a CLI backend's command line.
    pub args: Vec<String>,
    /// Give a CLI-backed master the bundled web and browser tooling: the
    /// github, context7 and playwright MCP servers, plus the CLI's own
    /// WebSearch/WebFetch. Off means telepager's own bridge and nothing else.
    // TODO: flipping the bundled_mcp default is out of scope for this round.
    #[serde(alias = "bundledMcp")]
    pub bundled_mcp: bool,
    /// Additional MCP servers for a CLI-backed master, in Claude Code's
    /// `mcpServers` shape. Merged in on top of the bundled set, and honoured
    /// whether or not `bundled_mcp` is on.
    #[serde(alias = "mcpServers")]
    pub mcp_servers: Option<serde_json::Map<String, serde_json::Value>>,
    /// Let the master see, read and type at coding agents running in tmux panes
    /// telepager didn't start. Off by default: those are the user's own
    /// terminals, so this reaches further than the spawned-session model does.
    #[serde(alias = "tmuxAttach")]
    pub tmux_attach: bool,
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
            bundled_mcp: true,
            mcp_servers: None,
            tmux_attach: false,
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

    /// Like `base_url_or_default`, but refuses a remote `http://` address, so a
    /// key is never sent in cleartext to someone else's server. Callers that
    /// use the address (the LLM client, the save paths) go through here.
    pub fn try_base_url_or_default(&self) -> Result<String> {
        let url = self.base_url_or_default();
        validate_base_url(&url)?;
        Ok(url)
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

/// A worker agent: some coding CLI we spawn in a directory with a task.
/// `{task}` in an arg is substituted; an arg that is exactly `{task}` is swapped
/// whole, so tasks with spaces and quotes survive without a shell.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPreset {
    pub command: String,
    pub args: Vec<String>,
    /// Shown in the UI picker.
    pub description: String,
    /// Extra env for the child, e.g. a per-agent API key.
    pub env: BTreeMap<String, String>,
    /// Variables to take away from the child before `env` is applied, for a
    /// CLI that would rather use an exported key than the login it already has.
    pub unset: Vec<String>,
    /// How this CLI is told which model to use, e.g. `["--model", "{model}"]`.
    /// Appended only when a spawn asks for one; empty means it can't be asked.
    pub model_args: Vec<String>,
    /// Whether this agent keeps reading stdin after it starts, which decides
    /// if "send a follow-up" is offered for its sessions.
    pub interactive: bool,
    /// Run it on a real terminal, so the console draws its screen instead of a
    /// line log. Off by default because we hold the terminal: a pty agent dies
    /// when telepager does, a piped one keeps going.
    pub pty: bool,
}

/// The coding CLIs we know out of the box. Anything on PATH shows up in the UI
/// with no configuration; add the rest under `agents` in the config.
pub fn builtin_agents() -> BTreeMap<String, AgentPreset> {
    let mut m = BTreeMap::new();
    // `watch` picks the pair that go together: a real terminal to draw on, and
    // stdin left open so you can type a follow-up at it.
    let mut add = |name: &str, command: &str, args: &[&str], watch: bool, description: &str| {
        m.insert(
            name.to_string(),
            AgentPreset {
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                description: description.to_string(),
                env: BTreeMap::new(),
                unset: Vec::new(),
                model_args: Vec::new(),
                interactive: watch,
                pty: watch,
            },
        );
    };

    add("claude", "claude", &["-p", "{task}", "--permission-mode", "acceptEdits"], false,
        "Claude Code, headless");
    add("codex", "codex", &["exec", "--full-auto", "{task}"], false,
        "OpenAI Codex CLI");
    add("gemini", "gemini", &["-y", "-p", "{task}"], false,
        "Google Gemini CLI");
    add("opencode", "opencode", &["run", "{task}"], false,
        "opencode, any model it's configured with");
    add("cursor", "cursor-agent", &["-p", "{task}", "--force"], false,
        "Cursor CLI agent");
    add("amp", "amp", &["-x", "{task}"], false,
        "Sourcegraph Amp");
    add("aider", "aider", &["--yes", "--no-check-update", "--message", "{task}"], false,
        "aider, any model it's configured with");
    add("crush", "crush", &["run", "{task}"], false,
        "Charm Crush");
    add("qwen", "qwen", &["-y", "-p", "{task}"], false,
        "Qwen Code CLI");
    add("goose", "goose", &["run", "-t", "{task}"], false,
        "Block Goose");

    // the same agents, but on screen. telepager holds the terminal, so unlike
    // the headless ones above these end when telepager does.
    add("claude-tui", "claude", &["{task}"], true,
        "Claude Code, on screen — ends when telepager does");
    add("codex-tui", "codex", &["{task}"], true,
        "Codex, on screen — ends when telepager does");
    add("gemini-tui", "gemini", &["-i", "{task}"], true,
        "Gemini, on screen — ends when telepager does");
    add("opencode-tui", "opencode", &["{task}"], true,
        "opencode, on screen — ends when telepager does");

    // the clis whose model flag we're sure of. anything else can say so in the
    // config rather than have us guess a flag at it
    for name in [
        "claude", "claude-tui", "codex", "codex-tui", "gemini", "gemini-tui",
        "opencode", "opencode-tui", "aider",
    ] {
        if let Some(preset) = m.get_mut(name) {
            preset.model_args = vec!["--model".into(), "{model}".into()];
        }
    }

    // a claude worker is the claude code login, the same way the master's
    // claude-code backend is. an exported key would quietly replace it, and
    // headless there's no prompt to notice. put one back with `env` if that's
    // what you actually want.
    for preset in m.values_mut() {
        if preset.command == "claude" {
            preset.unset = vec!["ANTHROPIC_API_KEY".into(), "ANTHROPIC_AUTH_TOKEN".into()];
        }
    }
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
    /// Directories telegram may spawn agents in; empty means it can't spawn at
    /// all. The local web UI isn't restricted — reaching it means being here.
    pub allowed_dirs: Vec<PathBuf>,
    pub ui_port: u16,
    pub permissions: Permissions,
}

impl Config {
    /// Every chat we page, primary first. `chat_id` stays the default so old
    /// config files keep working; the rest get the same messages.
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
// $XDG_CONFIG_HOME or ~/.config on linux. ~/.config stays a fallback across
// unix because that's what the readme has always said.
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
    resolve_config_path_in(explicit, Path::new("."), config_candidates())
}

/// The lookup order, with the working directory and global candidates passed
/// in so it can be tested without changing process-wide state:
/// `--config` > `./.telepager/config.json` > `./telepager.config.json` > global.
fn resolve_config_path_in(
    explicit: Option<&Path>,
    cwd: &Path,
    globals: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }

    let project = cwd.join(".telepager").join("config.json");
    if project.exists() {
        return Some(project);
    }

    let legacy = cwd.join("telepager.config.json");
    if legacy.exists() {
        return Some(legacy);
    }

    globals.into_iter().find(|p| p.exists())
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

/// The config file as raw JSON, so a caller can edit one key and write the rest
/// back untouched.
fn read_doc(path: &Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(_) => Ok(serde_json::json!({})),
    }
}

/// Write `bytes` to `path` atomically and owner-only: tmp file in the same
/// directory, then rename over the target.
///
/// The rename is the point. Opening `path` with truncate would follow a
/// symlink planted there and truncate its victim; rename replaces the link
/// itself, so the victim is never touched. Used for every file that carries a
/// secret (config, endpoint).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "telepager".to_string());
    let tmp = path.with_file_name(format!("{file_name}.tmp.{}", std::process::id()));

    let written = (|| -> Result<()> {
        // owner-only from creation, so the secret never sits in a
        // world-readable file between the open and the chmod
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(bytes).with_context(|| format!("writing {}", tmp.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("writing {}", tmp.display()))?;
        }

        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    })();

    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn write_doc(path: &Path, doc: &serde_json::Value) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(doc)?)
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

/// A remote model address must be https; http is only allowed to localhost,
/// so a key the master sends is never cleartext to a remote server. An empty
/// address means the provider default and is always fine here.
pub fn validate_base_url(url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("base_url must start with https:// (http:// is only allowed to localhost): {url}");
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" => {
            let host = base_url_host(rest);
            if is_local_host(&host) {
                Ok(())
            } else {
                bail!(
                    "refusing to send an api key over http to {host} — use https://, \
                     or a localhost address (127.0.0.1, localhost, ::1)"
                )
            }
        }
        _ => bail!("base_url must be https:// (http:// is only allowed to localhost): {url}"),
    }
}

fn base_url_host(rest: &str) -> String {
    let authority = rest.split('/').next().unwrap_or("");
    // drop any userinfo in front of the host
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(v6) = authority.strip_prefix('[') {
        if let Some(end) = v6.find(']') {
            return v6[..end].to_ascii_lowercase();
        }
        return authority.to_ascii_lowercase();
    }
    authority.split(':').next().unwrap_or(authority).to_ascii_lowercase()
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Replace just the `master` block, leaving the rest of the file alone.
pub fn save_master(explicit: Option<&Path>, master: &MasterConfig) -> Result<PathBuf> {
    if let Some(url) = master.base_url.as_deref() {
        validate_base_url(url)?;
    }
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

/// Replace the permissions block, leaving the rest of the file alone.
pub fn save_permissions(explicit: Option<&Path>, permissions: &Permissions) -> Result<PathBuf> {
    let path = target_path(explicit);
    let mut doc = read_doc(&path)?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    obj.insert("permissions".into(), serde_json::to_value(permissions)?);
    write_doc(&path, &doc)?;
    Ok(path)
}

/// Write a project-local `.telepager/config.json` that scopes terminal access
/// to `dir`, and keep the directory out of git. Anything already in the file —
/// the bot token and allowlist in particular — is left alone.
pub fn init(dir: Option<&Path>, preset: InitPreset) -> Result<PathBuf> {
    let base = match dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("finding the current directory")?,
    };
    let base = std::fs::canonicalize(&base)
        .with_context(|| format!("{} is not a directory", base.display()))?;
    if !base.is_dir() {
        bail!("{} is not a directory", base.display());
    }

    let path = base.join(".telepager").join("config.json");
    let mut doc = read_doc(&path)?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    obj.insert("allowed_dirs".into(), serde_json::json!([base]));
    obj.insert("permissions".into(), serde_json::to_value(preset.permissions())?);
    write_doc(&path, &doc)?;

    ignore_in_git(&base)?;
    Ok(path)
}

fn ignore_in_git(dir: &Path) -> Result<()> {
    let path = dir.join(".gitignore");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|line| line.trim() == ".telepager/") {
        return Ok(());
    }

    let mut out = text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(".telepager/\n");
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `provider: "openrouter"` names a service, not just an api shape. serde maps
/// the alias onto the provider and the name is gone, so the address it implies
/// has to be read back out of the file — otherwise an openrouter key is sent
/// to openai.
fn apply_named_service(text: &str, raw: &mut RawConfig) {
    let Some(master) = raw.master.as_mut() else { return };
    if master.base_url.as_deref().map(str::trim).is_some_and(|b| !b.is_empty()) {
        return;
    }
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else { return };
    let Some(name) = doc["master"]["provider"].as_str() else { return };
    if let Some((_, Some(base))) = Provider::named(name) {
        master.base_url = Some(base.to_string());
    }
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    load_inner(explicit, Path::new("."), config_candidates())
}

/// Read one config file into its raw shape, resolving named services.
fn read_raw_config(path: &Path) -> Result<RawConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(RawConfig::default());
    }
    let mut raw: RawConfig = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    apply_named_service(&text, &mut raw);
    Ok(raw)
}

/// Whether `path` is a project-local config (`./.telepager/config.json` or
/// `./telepager.config.json` in `cwd`).
fn is_project_path(path: &Path, cwd: &Path) -> bool {
    *path == *cwd.join(".telepager").join("config.json")
        || *path == *cwd.join("telepager.config.json")
}

/// The permissions a project file may effectively grant when a global config
/// also exists: the intersection of grants. Asking before destructive commands
/// is a restriction rather than a grant, so either side can reinstate it.
fn intersect_permissions(project: Permissions, global: Permissions) -> Permissions {
    Permissions {
        shell: project.shell && global.shell,
        remote_control: project.remote_control && global.remote_control,
        confirm_destructive: project.confirm_destructive || global.confirm_destructive,
    }
}

/// `load` with the working directory and global candidates supplied, so the
/// project-vs-global trust split can be tested without touching cwd.
///
/// Trust split: when the winning file is project-local, its `bot_token` and
/// `allowed_user_ids` are ignored in favour of the global file (or the
/// environment) whenever a global config exists — a file anyone can commit to
/// a repo must not supply the bot token or the whole allowlist. Project
/// `master`/`agents` are ignored entirely in that case (a committed
/// `master.command`/`agents.*.command` is arbitrary code execution); only
/// `allowed_dirs` and the intersected permissions are honoured from the
/// project file, so `init` + `setup` still works once a global config exists.
/// When NO global config exists, a project-local file is not trusted at all:
/// it loads only if token/ids come from the environment (and then only the
/// env credentials — every privileged field from the project file is
/// stripped), or if `--config` explicitly points at it. Otherwise we bail
/// guiding the user to `telepager setup`.
fn load_inner(explicit: Option<&Path>, cwd: &Path, globals: Vec<PathBuf>) -> Result<Config> {
    let env_token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
    let env_ids = std::env::var("TELEGRAM_ALLOWED_IDS").ok();
    load_inner_with_env(explicit, cwd, globals, env_token, env_ids)
}

/// `load_inner` with the environment supplied, so tests can exercise the
/// env-trust path without mutating process-wide env vars (which would race
/// other tests running in parallel).
fn load_inner_with_env(
    explicit: Option<&Path>,
    cwd: &Path,
    globals: Vec<PathBuf>,
    env_token: Option<String>,
    env_ids: Option<String>,
) -> Result<Config> {
    let file = resolve_config_path_in(explicit, cwd, globals.clone());

    let raw: RawConfig = match &file {
        Some(path) if path.exists() => read_raw_config(path)?,
        _ => RawConfig::default(),
    };

    let is_project = explicit.is_none()
        && file.as_deref().is_some_and(|p| is_project_path(p, cwd));
    let global_raw: Option<RawConfig> = if is_project {
        match globals.iter().find(|p| p.exists()) {
            Some(path) => Some(read_raw_config(path)?),
            None => None,
        }
    } else {
        None
    };

    let env_token = env_token.filter(|v| !v.trim().is_empty());
    let env_ids_present = env_ids.as_deref().is_some_and(|v| !v.trim().is_empty());

    // Project-local winner with no global config and no explicit --config:
    // never trust privileged fields from the project file. The only way in
    // is env credentials, and even then the project file's privileged fields
    // (bot_token/allowed_user_ids/master/agents/allowed_dirs/permissions)
    // are stripped — token/ids come from env, everything else is default.
    if is_project && global_raw.is_none() {
        if env_token.is_none() || !env_ids_present {
            let path = file
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "project config".to_string());
            bail!(
                "project config exists at {path} but no global config - run `telepager setup` or pass --config {path} to trust it"
            );
        }
        let token = env_token.unwrap();
        let allowed_user_ids = parse_allowed_ids(env_ids.as_deref().unwrap())?;
        if allowed_user_ids.is_empty() {
            bail!(
                "allowed_user_ids is empty. This is the whole security model — refusing \
                 to start with an empty allowlist. Set TELEGRAM_ALLOWED_IDS or add \
                 allowed_user_ids to your config file."
            );
        }
        let chat_id = *allowed_user_ids.iter().min().unwrap();
        return Ok(Config {
            token,
            allowed_user_ids,
            chat_id,
            ask_timeout_seconds: 300,
            master: MasterConfig::default(),
            agents: builtin_agents(),
            allowed_dirs: Vec::new(),
            ui_port: DEFAULT_UI_PORT,
            permissions: Permissions::default(),
        });
    }

    let token = match env_token {
        Some(v) => v,
        None => match &global_raw {
            Some(global) => global.bot_token.clone().unwrap_or_default(),
            None => raw.bot_token.clone().unwrap_or_default(),
        },
    };
    if token.trim().is_empty() {
        let where_to_put_it = first_candidate();
        bail!(
            "No bot token found. Set TELEGRAM_BOT_TOKEN or put bot_token in the \
             config file ({where_to_put_it})."
        );
    }

    let allowed_user_ids = match env_ids {
        Some(v) if !v.trim().is_empty() => parse_allowed_ids(&v)?,
        _ => match &global_raw {
            Some(global) => global.allowed_user_ids.clone().unwrap_or_default(),
            None => raw.allowed_user_ids.clone().unwrap_or_default(),
        },
    };
    if allowed_user_ids.is_empty() {
        bail!(
            "allowed_user_ids is empty. This is the whole security model — refusing \
             to start with an empty allowlist. Set TELEGRAM_ALLOWED_IDS or add \
             allowed_user_ids to your config file."
        );
    }

    // When a global config exists alongside a project winner, the global
    // file is the trusted source for master/agents/chat/timeout/port; the
    // project file only contributes allowed_dirs and narrowed permissions.
    // When the winner is not a project file, `raw` is the trusted source.
    let trusted: &RawConfig = match &global_raw {
        Some(global) if is_project => global,
        _ => &raw,
    };

    // in a private chat the chat id is just the user id
    let chat_id = trusted
        .chat_id
        .unwrap_or_else(|| *allowed_user_ids.iter().min().unwrap());

    // configured agents override a builtin of the same name, and add new ones
    // (from the trusted source only — project agents are ignored above)
    let mut agents = builtin_agents();
    for (name, preset) in trusted.agents.clone().unwrap_or_default() {
        agents.insert(name, preset);
    }

    let permissions = match (raw.permissions.clone(), global_raw.as_ref()) {
        (Some(project), Some(global)) if is_project => intersect_permissions(
            project,
            global.permissions.clone().unwrap_or_default(),
        ),
        (Some(project), None) => project,
        (None, Some(global)) if is_project => intersect_permissions(
            Permissions::default(),
            global.permissions.clone().unwrap_or_default(),
        ),
        (None, None) => Permissions::default(),
        (Some(project), Some(_)) => project,
        (None, Some(global)) => global.permissions.clone().unwrap_or_default(),
    };

    // allowed_dirs: honoured from the project file when a global config
    // exists (the init+setup flow), otherwise from the trusted source.
    let allowed_dirs = if is_project && global_raw.is_some() {
        raw.allowed_dirs.clone().unwrap_or_default()
    } else {
        trusted.allowed_dirs.clone().unwrap_or_default()
    };

    Ok(Config {
        token,
        allowed_user_ids,
        chat_id,
        ask_timeout_seconds: trusted.ask_timeout_seconds.unwrap_or(300),
        master: trusted.master.clone().unwrap_or_default(),
        agents,
        allowed_dirs,
        ui_port: trusted.ui_port.unwrap_or(DEFAULT_UI_PORT),
        permissions,
    })
}

/// Like `load`, but a missing or half-finished config is `None`, not an error.
/// The app has to start unconfigured — that's how it can serve its setup page.
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
            permissions: Default::default(),
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

    #[cfg(unix)]
    #[test]
    fn a_saved_config_file_is_owner_only() {
        let dir = scratch("cfg-mode");
        let path = dir.join("config.json");
        save(Some(&path), "tok", &[1]).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
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
    fn a_service_named_in_the_file_gets_its_address_too() {
        let dir = scratch("named-service");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"bot_token":"1:x","allowed_user_ids":[7],
                "master":{"provider":"openrouter","api_key":"sk-or-x"}}"#,
        )
        .unwrap();

        let cfg = load(Some(&path)).unwrap();
        assert_eq!(cfg.master.provider, Provider::Openai);
        // without this an openrouter key would be sent to openai
        assert_eq!(cfg.master.base_url_or_default(), "https://openrouter.ai/api/v1");

        // and a base_url written by hand still wins
        std::fs::write(
            &path,
            r#"{"bot_token":"1:x","allowed_user_ids":[7],
                "master":{"provider":"openrouter","base_url":"http://localhost:8080"}}"#,
        )
        .unwrap();
        assert_eq!(load(Some(&path)).unwrap().master.base_url_or_default(), "http://localhost:8080");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn naming_a_service_picks_the_provider_and_the_address_together() {
        let (provider, base) = Provider::named("openrouter").unwrap();
        assert_eq!(provider, Provider::Openai);
        assert_eq!(base, Some("https://openrouter.ai/api/v1"));

        // a plain provider name has no address of its own to add
        let (provider, base) = Provider::named("Anthropic").unwrap();
        assert_eq!(provider, Provider::Anthropic);
        assert_eq!(base, None);

        // and the aliases serde already knows still work
        assert_eq!(Provider::named("claude").unwrap().0, Provider::ClaudeCode);
        assert!(Provider::named("nonesuch").is_none());
    }

    #[test]
    fn the_claude_presets_run_on_the_login_not_an_exported_key() {
        let m = builtin_agents();
        for name in ["claude", "claude-tui"] {
            assert!(
                m[name].unset.contains(&"ANTHROPIC_API_KEY".to_string()),
                "{name} would inherit a key"
            );
        }
        // and nothing else is touched — those clis have their own arrangements
        assert!(m["codex"].unset.is_empty());
        assert!(m["opencode"].unset.is_empty());
    }

    #[test]
    fn the_agents_we_know_the_model_flag_for_can_be_asked_for_one() {
        let m = builtin_agents();
        assert_eq!(m["claude"].model_args, vec!["--model", "{model}"]);
        assert_eq!(m["codex-tui"].model_args, vec!["--model", "{model}"]);
        // and the ones we'd only be guessing at are left alone
        assert!(m["amp"].model_args.is_empty());
        assert!(m["goose"].model_args.is_empty());
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
    fn base_url_requires_https_for_remote_hosts() {
        // remote http would send the key in cleartext, so it's refused
        assert!(validate_base_url("http://evil.example/v1").is_err());
        assert!(validate_base_url("http://10.0.0.1:8080/v1").is_err());
        assert!(validate_base_url("http://127.0.0.1.evil.com/v1").is_err());
        assert!(validate_base_url("ftp://api.example/v1").is_err());

        // localhost http and https anywhere are fine
        for url in [
            "http://127.0.0.1:8080/v1",
            "http://localhost:8080",
            "http://localhost",
            "http://[::1]:8080/v1",
            "https://api.example/v1",
            "https://openrouter.ai/api/v1",
            "http://127.0.0.1:11434/v1",
            "",
        ] {
            assert!(validate_base_url(url).is_ok(), "wrongly refused: {url}");
        }
    }

    #[test]
    fn saving_master_refuses_cleartext_http_base_url() {
        let dir = scratch("baseurl-http");
        let path = dir.join("config.json");
        save(Some(&path), "tok", &[1]).unwrap();

        let bad = MasterConfig {
            provider: Provider::Openai,
            base_url: Some("http://evil.example/v1".into()),
            ..MasterConfig::default()
        };
        let err = save_master(Some(&path), &bad).unwrap_err();
        assert!(format!("{err:#}").contains("http"), "{err:#}");

        // and a localhost address still saves fine
        let good = MasterConfig {
            provider: Provider::Openai,
            base_url: Some("http://127.0.0.1:8080/v1".into()),
            ..MasterConfig::default()
        };
        save_master(Some(&path), &good).unwrap();
        assert_eq!(
            load(Some(&path)).unwrap().master.base_url_or_default(),
            "http://127.0.0.1:8080/v1"
        );
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn nothing_is_granted_until_someone_grants_it() {
        let p = Permissions::default();
        assert!(!p.shell);
        assert!(!p.remote_control);
        // the one that's on by default is the one that asks first
        assert!(p.confirm_destructive);
    }

    // a config file written before permissions existed has to keep working,
    // and has to keep meaning "nothing granted"
    #[test]
    fn an_old_config_file_still_loads_with_everything_off() {
        let dir = scratch("perms-old");
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"bot_token":"t","allowed_user_ids":[1],"ui_port":123}"#).unwrap();

        let cfg = load(Some(&path)).unwrap();
        assert!(!cfg.permissions.shell);
        assert!(!cfg.permissions.remote_control);
        assert!(cfg.permissions.confirm_destructive);
        assert_eq!(cfg.ui_port, 123);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_half_written_permissions_block_fills_in_the_rest() {
        let dir = scratch("perms-partial");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"bot_token":"t","allowed_user_ids":[1],"permissions":{"shell":true}}"#,
        )
        .unwrap();

        let cfg = load(Some(&path)).unwrap();
        assert!(cfg.permissions.shell);
        assert!(!cfg.permissions.remote_control);
        assert!(cfg.permissions.confirm_destructive);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_permissions_leaves_the_rest_of_the_file_alone() {
        let dir = scratch("perms-save");
        let path = dir.join("config.json");
        save(Some(&path), "tok", &[1]).unwrap();

        save_permissions(
            Some(&path),
            &Permissions { shell: true, remote_control: true, confirm_destructive: false },
        )
        .unwrap();

        let cfg = load(Some(&path)).unwrap();
        assert_eq!(cfg.token, "tok");
        assert!(cfg.permissions.shell);
        assert!(cfg.permissions.remote_control);
        assert!(!cfg.permissions.confirm_destructive);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ollama_is_usable_without_a_key() {
        let m = MasterConfig { provider: Provider::Ollama, ..MasterConfig::default() };
        assert!(m.is_usable());
    }

    #[test]
    fn init_lookup_prefers_the_project_config() {
        let dir = scratch("init-lookup");
        let global = dir.join("global").join("config.json");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(&global, "{}").unwrap();

        assert_eq!(
            resolve_config_path_in(None, &dir, vec![global.clone()]),
            Some(global.clone())
        );

        let legacy = dir.join("telepager.config.json");
        std::fs::write(&legacy, "{}").unwrap();
        assert_eq!(
            resolve_config_path_in(None, &dir, vec![global.clone()]),
            Some(legacy.clone())
        );

        let project = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "{}").unwrap();
        assert_eq!(
            resolve_config_path_in(None, &dir, vec![global.clone()]),
            Some(project.clone())
        );

        let explicit = dir.join("explicit.json");
        assert_eq!(
            resolve_config_path_in(Some(&explicit), &dir, vec![global]),
            Some(explicit)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn init_writes_the_preset_permissions_and_the_allowed_dir() {
        let dir = scratch("init-preset");
        let path = init(Some(&dir), InitPreset::Full).unwrap();

        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["allowed_dirs"], serde_json::json!([dir.canonicalize().unwrap()]));
        assert_eq!(back["permissions"]["shell"], true);
        assert_eq!(back["permissions"]["remote_control"], true);
        assert_eq!(back["permissions"]["confirm_destructive"], true);

        let pager = scratch("init-pager");
        let path = init(Some(&pager), InitPreset::PagerOnly).unwrap();
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["permissions"]["shell"], false);
        assert_eq!(back["permissions"]["remote_control"], false);
        assert_eq!(back["permissions"]["confirm_destructive"], true);

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&pager).unwrap();
    }

    #[test]
    fn init_keeps_a_token_already_in_the_project_config() {
        let dir = scratch("init-merge");
        let path = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"bot_token":"keep","allowed_user_ids":[7]}"#).unwrap();

        init(Some(&dir), InitPreset::Full).unwrap();

        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["bot_token"], "keep");
        assert_eq!(back["allowed_user_ids"], serde_json::json!([7]));
        assert_eq!(back["permissions"]["shell"], true);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn init_ignores_the_project_config_in_git_once() {
        let dir = scratch("init-gitignore");
        std::fs::write(dir.join(".gitignore"), "target\n").unwrap();

        init(Some(&dir), InitPreset::PagerOnly).unwrap();
        init(Some(&dir), InitPreset::PagerOnly).unwrap();

        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(text.starts_with("target\n"), "{text}");
        assert_eq!(text.matches(".telepager/").count(), 1, "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saving_through_a_symlink_does_not_write_the_victim() {
        // a symlink planted at the config path (or swapped in mid-write) must
        // not turn the write into a truncate of its target. The victim is
        // valid (empty) JSON so the read side is not what stops it.
        let dir = scratch("symlink");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "{}").unwrap();
        let path = dir.join("config.json");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        save(Some(&path), "tok", &[1]).unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "{}");
        // the link itself was replaced by a real file
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["bot_token"], "tok");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn trust_case(name: &str, project: &str, global: &str) -> (PathBuf, Vec<PathBuf>) {
        let dir = scratch(name);
        let project_path = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(&project_path, project).unwrap();
        let global_path = dir.join("global").join("config.json");
        std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
        std::fs::write(&global_path, global).unwrap();
        (dir, vec![global_path])
    }

    #[test]
    fn a_project_token_is_ignored_when_a_global_config_exists() {
        // a project file anyone can commit must not hand over the bot token
        // or the allowlist when a global config is in play
        let (dir, globals) = trust_case(
            "trust-token",
            r#"{"bot_token":"project","allowed_user_ids":[1]}"#,
            r#"{"bot_token":"global","allowed_user_ids":[2]}"#,
        );
        let loaded = load_inner(None, &dir, globals).unwrap();
        assert_eq!(loaded.token, "global");
        assert_eq!(loaded.allowed_user_ids, vec![2]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_project_shell_grant_cannot_escalate_a_global_denial() {
        let (dir, globals) = trust_case(
            "trust-shell",
            r#"{"bot_token":"project","allowed_user_ids":[1],"permissions":{"shell":true,"remote_control":true}}"#,
            r#"{"bot_token":"global","allowed_user_ids":[2],"permissions":{"shell":false,"remote_control":false}}"#,
        );
        let loaded = load_inner(None, &dir, globals).unwrap();
        assert!(!loaded.permissions.shell);
        assert!(!loaded.permissions.remote_control);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_intersection_never_grants_what_the_project_denied() {
        let (dir, globals) = trust_case(
            "trust-shell2",
            r#"{"bot_token":"project","allowed_user_ids":[1],"permissions":{"shell":false}}"#,
            r#"{"bot_token":"global","allowed_user_ids":[2],"permissions":{"shell":true}}"#,
        );
        let loaded = load_inner(None, &dir, globals).unwrap();
        assert!(!loaded.permissions.shell);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fresh_project_only_config_still_loads() {
        // P0: no global + no explicit --config + no env => refuse, do not
        // trust the project token/permissions. (Was: fresh init loads.)
        let dir = scratch("trust-fresh");
        let project_path = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            r#"{"bot_token":"project","allowed_user_ids":[1],"permissions":{"shell":true}}"#,
        )
        .unwrap();
        let missing_global = dir.join("global").join("config.json");

        let err = load_inner_with_env(None, &dir, vec![missing_global], None, None).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("project config exists at"), "{text}");
        assert!(text.contains("telepager setup"), "{text}");
        assert!(text.contains("--config"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_project_token_is_accepted_when_there_is_no_global_config() {
        // P0: same refusal — a project token alone is not trust.
        let dir = scratch("trust-noglobal");
        let project_path = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(&project_path, r#"{"bot_token":"project","allowed_user_ids":[1]}"#).unwrap();
        let missing_global = dir.join("global").join("config.json");

        let err = load_inner_with_env(None, &dir, vec![missing_global], None, None).unwrap_err();
        assert!(format!("{err:#}").contains("project config exists at"), "{err:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fresh_clone_project_payload_is_refused_exactly() {
        // The exact fresh-clone payload: privileged fields must not load.
        let dir = scratch("trust-exact");
        let project_path = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            r#"{"bot_token":"evil","allowed_user_ids":[999],"master":{"provider":"claude-code","command":"evil-bin"},"agents":{"evil":{"command":"evil-agent","args":[]}},"allowed_dirs":["/tmp/evil"],"permissions":{"shell":true,"remote_control":true}}"#,
        )
        .unwrap();
        let missing_global = dir.join("global").join("config.json");
        let err = load_inner_with_env(None, &dir, vec![missing_global], None, None).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&format!(
                "project config exists at {} but no global config - run `telepager setup` or pass --config {} to trust it",
                project_path.display(),
                project_path.display()
            )),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_token_still_refuses_privileged_project_fields() {
        let dir = scratch("trust-env");
        let project_path = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            r#"{"bot_token":"evil","allowed_user_ids":[999],"master":{"provider":"openai","command":"evil-bin"},"agents":{"evil":{"command":"evil-agent","args":[]}},"allowed_dirs":["/tmp/evil"],"permissions":{"shell":true,"remote_control":true}}"#,
        )
        .unwrap();
        let missing_global = dir.join("global").join("config.json");
        let loaded = load_inner_with_env(
            None,
            &dir,
            vec![missing_global],
            Some("env-token".into()),
            Some("7".into()),
        )
        .unwrap();
        assert_eq!(loaded.token, "env-token");
        assert_eq!(loaded.allowed_user_ids, vec![7]);
        // privileged fields stripped
        assert!(loaded.master.command.is_none(), "{:?}", loaded.master.command);
        assert_eq!(loaded.master.provider, Provider::ClaudeCode);
        assert!(!loaded.agents.contains_key("evil"));
        assert!(loaded.allowed_dirs.is_empty(), "{:?}", loaded.allowed_dirs);
        assert!(!loaded.permissions.shell);
        assert!(!loaded.permissions.remote_control);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_explicit_trusts_the_project_file_fully() {
        let dir = scratch("trust-explicit");
        let project_path = dir.join(".telepager").join("config.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            r#"{"bot_token":"project","allowed_user_ids":[1],"master":{"provider":"claude-code","command":"my-claude"},"agents":{"mine":{"command":"mine","args":[]}},"allowed_dirs":["/tmp/ok"],"permissions":{"shell":true}}"#,
        )
        .unwrap();
        let missing_global = dir.join("global").join("config.json");
        let loaded = load_inner_with_env(
            Some(&project_path),
            &dir,
            vec![missing_global],
            None,
            None,
        )
        .unwrap();
        assert_eq!(loaded.token, "project");
        assert_eq!(loaded.master.command.as_deref(), Some("my-claude"));
        assert!(loaded.agents.contains_key("mine"));
        assert_eq!(loaded.allowed_dirs, vec![PathBuf::from("/tmp/ok")]);
        assert!(loaded.permissions.shell);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn init_setup_flow_honours_project_allowed_dirs_but_not_master() {
        // Global present => project allowed_dirs honoured, project
        // master/agents ignored.
        let (dir, globals) = trust_case(
            "trust-initsetup",
            r#"{"bot_token":"project","allowed_user_ids":[1],"master":{"provider":"claude-code","command":"evil-bin"},"agents":{"evil":{"command":"evil-agent","args":[]}},"allowed_dirs":["/tmp/projdir"],"permissions":{"shell":true}}"#,
            r#"{"bot_token":"global","allowed_user_ids":[2],"master":{"provider":"openai","model":"gpt-4o"},"permissions":{"shell":true}}"#,
        );
        let loaded = load_inner_with_env(None, &dir, globals, None, None).unwrap();
        assert_eq!(loaded.token, "global");
        assert_eq!(loaded.allowed_dirs, vec![PathBuf::from("/tmp/projdir")]);
        assert!(loaded.master.command.is_none(), "{:?}", loaded.master.command);
        assert_eq!(loaded.master.provider, Provider::Openai);
        assert!(!loaded.agents.contains_key("evil"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn provider_key_env_allowlist() {
        assert!(Provider::Openai.allows_key_env("OPENAI_API_KEY"));
        assert!(Provider::Openai.allows_key_env("OPENROUTER_API_KEY"));
        assert!(!Provider::Openai.allows_key_env("TELEGRAM_BOT_TOKEN"));
        assert!(!Provider::Openai.allows_key_env("ANTHROPIC_API_KEY"));
        assert!(!Provider::ClaudeCode.allows_key_env("OPENAI_API_KEY"));
        let err = Provider::Openai.validate_api_key_env("TELEGRAM_BOT_TOKEN").unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to use TELEGRAM_BOT_TOKEN as api key"),
            "{err:#}"
        );
        assert!(Provider::Openai.validate_api_key_env("OPENROUTER_API_KEY").is_ok());
    }
}
