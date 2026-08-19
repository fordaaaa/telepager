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

// write bot_token and allowed_user_ids without stomping on anything else
// the file already had (ask_timeout_seconds, chat_id, comments are lost but
// those are the only keys we've ever documented)
pub fn save(explicit: Option<&Path>, token: &str, ids: &[i64]) -> Result<PathBuf> {
    let path = target_path(explicit);

    let mut doc = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(_) => serde_json::json!({}),
    };
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;

    obj.insert("bot_token".into(), serde_json::json!(token));
    obj.insert("allowed_user_ids".into(), serde_json::json!(ids));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)
        .with_context(|| format!("writing {}", path.display()))?;

    // the token is a credential, so don't leave it world readable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(path)
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

    #[test]
    fn save_keeps_other_keys() {
        let dir = std::env::temp_dir().join(format!("telepager-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("telepager-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("config.json");

        save(Some(&path), "tok", &[1]).unwrap();

        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["bot_token"], "tok");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn explicit_path_wins() {
        let p = Path::new("/some/where/cfg.json");
        assert_eq!(resolve_config_path(Some(p)), Some(p.to_path_buf()));
    }
}
