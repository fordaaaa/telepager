use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

// where the daemon writes its port and token so clients can find it
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// Where the web console is, so `telepager webui` can just open it.
    pub ui_port: Option<u16>,
    pub ui_key: Option<String>,
}

impl Endpoint {
    pub fn ui_url(&self) -> Option<String> {
        let port = self.ui_port?;
        let key = self.ui_key.as_deref()?;
        Some(format!("http://127.0.0.1:{port}/?k={key}"))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello { token: String, label: String, #[serde(default)] cwd: Option<PathBuf> },
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
    write_endpoint_to(&state_path(), e)
}

fn write_endpoint_to(path: &Path, e: &Endpoint) -> Result<()> {
    // tmp-then-rename (config::write_atomic): a symlink at the endpoint path
    // is replaced, never followed to its victim
    crate::config::write_atomic(path, &serde_json::to_vec_pretty(e)?)
}

// the path to tell someone to write their config to
pub fn config_hint() -> String {
    crate::config::first_candidate()
}

pub fn read_endpoint() -> Option<Endpoint> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear_endpoint() {
    let _ = std::fs::remove_file(state_path());
}

/// Clear the endpoint file only if it still describes this process. A daemon
/// that failed to start must not delete the running one's address — every mcp
/// shim and the master's bridge resolve the daemon through this file.
pub fn clear_endpoint_if_ours() {
    if read_endpoint().map(|e| e.pid) == Some(std::process::id()) {
        clear_endpoint();
    }
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
    // two rounds of the os-seeded hasher, plenty for a value that lives in a
    // 0600 file on this machine. TODO: a CSPRNG 32-byte token would be stronger.
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

    #[cfg(unix)]
    #[test]
    fn an_endpoint_file_is_owner_only_from_creation() {
        let dir =
            std::env::temp_dir().join(format!("telepager-ipc-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("daemon.json");

        let e = Endpoint {
            port: 44013,
            token: "tok".into(),
            pid: std::process::id(),
            ui_port: Some(46375),
            ui_key: Some("k".into()),
        };
        write_endpoint_to(&path, &e).unwrap();

        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // and it round-trips
        let back: Endpoint = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.port, 44013);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn writing_an_endpoint_through_a_symlink_does_not_write_the_victim() {
        let dir =
            std::env::temp_dir().join(format!("telepager-ipc-sym-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        let path = dir.join("daemon.json");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let e = Endpoint {
            port: 44013,
            token: "tok".into(),
            pid: std::process::id(),
            ui_port: Some(46375),
            ui_key: Some("k".into()),
        };
        write_endpoint_to(&path, &e).unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        let back: Endpoint = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.port, 44013);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
