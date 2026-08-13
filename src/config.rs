use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    bot_token: Option<String>,
    allowed_user_ids: Option<Vec<i64>>,
    chat_id: Option<i64>,
    ask_timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    pub allowed_user_ids: Vec<i64>,
    pub chat_id: i64,
    pub ask_timeout_seconds: u64,
}

fn home_config() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/telepager/config.json")
}

fn resolve_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }

    let cwd = PathBuf::from("telepager.config.json");
    if cwd.exists() {
        return Some(cwd);
    }

    let home = home_config();
    home.exists().then_some(home)
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let file = resolve_config_path(explicit);

    let raw: RawConfig = match &file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", path.display()))?
        }
        None => RawConfig::default(),
    };

    let token = match std::env::var("TELEGRAM_BOT_TOKEN") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => raw.bot_token.clone().unwrap_or_default(),
    };
    if token.trim().is_empty() {
        bail!(
            "No bot token found. Set TELEGRAM_BOT_TOKEN or put bot_token in the \
             config file (~/.config/telepager/config.json)."
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

    Ok(Config {
        token,
        allowed_user_ids,
        chat_id,
        ask_timeout_seconds: raw.ask_timeout_seconds.unwrap_or(300),
    })
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
    fn explicit_path_wins() {
        let p = Path::new("/some/where/cfg.json");
        assert_eq!(resolve_config_path(Some(p)), Some(p.to_path_buf()));
    }
}
