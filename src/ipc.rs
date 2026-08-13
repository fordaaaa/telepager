use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// where the daemon writes its port and token so clients can find it
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello { token: String, label: String },
    Send { text: String },
    Thinking { text: String },
    Ask { question: String, options: Vec<String> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub result: String,
}

pub fn state_path() -> PathBuf {
    let dir = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("telepager").join("daemon.json")
}

pub fn write_endpoint(e: &Endpoint) -> Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(e)?)
        .with_context(|| format!("writing {}", path.display()))?;

    // the token is what stops other local users talking to the daemon
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn read_endpoint() -> Option<Endpoint> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear_endpoint() {
    let _ = std::fs::remove_file(state_path());
}

// a label for the session, so messages can say which project is asking
pub fn session_label() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "telepager".into())
}

pub fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // two rounds of the os-seeded hasher, plenty for a value that only lives
    // in a 0600 file on the same machine
    let a = RandomState::new().build_hasher().finish();
    let b = RandomState::new().build_hasher().finish();
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_not_predictable() {
        assert_ne!(new_token(), new_token());
        assert_eq!(new_token().len(), 32);
    }

    #[test]
    fn requests_round_trip() {
        let r = Request::Ask {
            question: "which one".into(),
            options: vec!["a".into(), "b".into()],
        };
        let text = serde_json::to_string(&r).unwrap();
        assert!(text.contains("\"op\":\"ask\""));
        match serde_json::from_str::<Request>(&text).unwrap() {
            Request::Ask { options, .. } => assert_eq!(options.len(), 2),
            other => panic!("round tripped into {other:?}"),
        }
    }

    #[test]
    fn state_path_is_namespaced() {
        assert!(state_path().ends_with("telepager/daemon.json"));
    }
}
