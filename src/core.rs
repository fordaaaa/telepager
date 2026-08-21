//! The one thing that owns everything.
//!
//! Core holds the Telegram connection, the session registry, the processes we
//! spawned and the master agent's conversation. The IPC socket, the web UI and
//! the Telegram poller are all clients of it, so there is exactly one place
//! where a question gets routed or an agent gets started.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use crate::agents;
use crate::config::{self, Config};
use crate::session::{Event, EventKind, Kind, Registry, SessionId, State};
use crate::telegram::Telegram;

/// Cap on options in a question — Telegram keyboards get unusable past this.
pub const MAX_OPTIONS: usize = 20;
/// Finished sessions kept for scrollback before they're dropped.
const KEEP_FINISHED: usize = 30;
/// Output lines quoted back to Telegram when an agent finishes.
const EXIT_TAIL_LINES: usize = 15;

#[derive(Debug)]
pub enum Answer {
    Tapped(usize),
    Typed(String),
}

/// A question blocked on the user, answerable from Telegram or the web UI.
struct Pending {
    session: SessionId,
    question: String,
    options: Vec<String>,
    /// The Telegram message carrying the buttons, so a tap can find it.
    tg_message_id: Option<i64>,
    tx: oneshot::Sender<Answer>,
}

/// A running agent process, kept so it can be written to and killed.
///
/// The child sits behind its own lock so the reaper can poll it without
/// holding the whole process table while an agent runs for an hour.
struct AgentProcess {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
}

/// Where the web UI is listening, published so `telepager webui` can find it.
#[derive(Debug, Clone)]
pub struct UiInfo {
    pub port: u16,
    pub key: String,
}

impl UiInfo {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?k={}", self.port, self.key)
    }
}

pub struct Core {
    /// An explicit --config, carried so saves land in the same file.
    pub config_path: Option<PathBuf>,
    /// None until Telegram is set up. The app runs unconfigured on purpose:
    /// that's what lets it serve its own setup page.
    cfg: RwLock<Option<Config>>,
    tg: RwLock<Option<Arc<Telegram>>>,
    registry: Mutex<Registry>,
    pending: Mutex<HashMap<String, Pending>>,
    processes: Mutex<HashMap<SessionId, AgentProcess>>,
    /// Bumped per question so ids are unique for the lifetime of the process.
    next_question: Mutex<u64>,
    /// The master agent's conversation, if it's been used.
    pub master: Mutex<crate::master::Conversation>,
    /// Woken when the config changes, so the Telegram poller restarts.
    pub config_changed: Notify,
    ui: RwLock<Option<UiInfo>>,
}

impl Core {
    pub fn new(config_path: Option<PathBuf>) -> Arc<Self> {
        let cfg = config::load_optional(config_path.as_deref());
        let tg = cfg
            .as_ref()
            .and_then(|c| Telegram::new(&c.token).ok())
            .map(Arc::new);

        Arc::new(Self {
            config_path,
            cfg: RwLock::new(cfg),
            tg: RwLock::new(tg),
            registry: Mutex::new(Registry::new()),
            pending: Mutex::new(HashMap::new()),
            processes: Mutex::new(HashMap::new()),
            next_question: Mutex::new(0),
            master: Mutex::new(crate::master::Conversation::default()),
            config_changed: Notify::new(),
            ui: RwLock::new(None),
        })
    }

    // ------------------------------------------------------------ config

    pub async fn config(&self) -> Option<Config> {
        self.cfg.read().await.clone()
    }

    pub async fn is_configured(&self) -> bool {
        self.cfg.read().await.is_some()
    }

    pub async fn telegram(&self) -> Option<Arc<Telegram>> {
        self.tg.read().await.clone()
    }

    /// Re-read the config file and swap in a fresh Telegram client. Called
    /// after the setup page writes a token, so nothing has to be restarted.
    pub async fn reload_config(&self) -> bool {
        let fresh = config::load_optional(self.config_path.as_deref());
        let changed = match (&fresh, &*self.cfg.read().await) {
            (Some(a), Some(b)) => a.token != b.token || a.allowed_user_ids != b.allowed_user_ids,
            (a, b) => a.is_some() != b.is_some(),
        };

        let tg = fresh
            .as_ref()
            .and_then(|c| Telegram::new(&c.token).ok())
            .map(Arc::new);
        *self.cfg.write().await = fresh;
        *self.tg.write().await = tg;

        if changed {
            self.notice("config reloaded").await;
            self.config_changed.notify_waiters();
        }
        changed
    }

    pub async fn set_ui(&self, info: UiInfo) {
        *self.ui.write().await = Some(info);
    }

    pub async fn ui(&self) -> Option<UiInfo> {
        self.ui.read().await.clone()
    }

    // ---------------------------------------------------------- sessions

    pub async fn open_session(
        &self,
        kind: Kind,
        label: String,
        cwd: Option<PathBuf>,
        agent: Option<String>,
        task: Option<String>,
    ) -> SessionId {
        let mut reg = self.registry.lock().await;
        reg.prune(KEEP_FINISHED);
        reg.open(kind, label, cwd, agent, task)
    }

    pub async fn close_session(&self, id: &str, code: Option<i32>) {
        self.registry.lock().await.close(id, code);
    }

    pub async fn set_state(&self, id: &str, state: State) {
        self.registry.lock().await.set_state(id, state);
    }

    pub async fn sessions(&self) -> Vec<Value> {
        self.registry.lock().await.summaries()
    }

    pub async fn session_lines(&self) -> Vec<String> {
        self.registry.lock().await.list().iter().map(|s| s.line()).collect()
    }

    pub async fn live_count(&self) -> usize {
        self.registry.lock().await.live().len()
    }

    pub async fn snapshot(&self, session: Option<&str>, limit: usize) -> Vec<Event> {
        self.registry.lock().await.snapshot(session, limit)
    }

    pub async fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.registry.lock().await.subscribe()
    }

    pub async fn record(&self, session: Option<&str>, kind: EventKind) {
        self.registry.lock().await.record(session, kind);
    }

    pub async fn notice(&self, text: &str) {
        self.record(None, EventKind::Notice { text: text.to_string() }).await;
    }

    pub async fn output_tail(&self, id: &str, lines: usize) -> Option<String> {
        self.registry.lock().await.get(id).map(|s| s.output_tail(lines))
    }

    pub async fn session_label(&self, id: &str) -> Option<String> {
        self.registry.lock().await.get(id).map(|s| s.label.clone())
    }

    pub async fn forget_session(&self, id: &str) -> bool {
        self.registry.lock().await.forget(id)
    }

    // ----------------------------------------------------------- paging

    /// Send a message to Telegram on a session's behalf.
    pub async fn send(&self, session: Option<&str>, text: &str) -> String {
        let label = match session {
            Some(id) => self.session_label(id).await.unwrap_or_else(|| "telepager".into()),
            None => "telepager".into(),
        };

        if let Some(id) = session {
            self.registry
                .lock()
                .await
                .record(Some(id), EventKind::Message { text: text.to_string() });
        }

        let (Some(tg), Some(cfg)) = (self.telegram().await, self.config().await) else {
            return "not set up yet — run `telepager setup`".into();
        };
        match tg.send_message(cfg.chat_id, &decorate(&label, text)).await {
            Ok(_) => "sent".into(),
            Err(e) => format!("could not send: {e:#}"),
        }
    }

    /// The in-place status line. `previous` is the message being edited.
    pub async fn thinking(&self, session: Option<&str>, text: &str, previous: Option<i64>) -> (String, Option<i64>) {
        let label = match session {
            Some(id) => self.session_label(id).await.unwrap_or_else(|| "telepager".into()),
            None => "telepager".into(),
        };
        if let Some(id) = session {
            self.registry.lock().await.set_thinking(id, text);
        }

        let (Some(tg), Some(cfg)) = (self.telegram().await, self.config().await) else {
            return ("not set up yet — run `telepager setup`".into(), previous);
        };

        let body = decorate(&label, &format!("💭 {text}"));
        if let Some(id) = previous {
            match tg.edit_message(cfg.chat_id, id, &body).await {
                Ok(()) => return ("updated".into(), Some(id)),
                Err(e) => log::debug!("status edit failed, sending a new one: {e:#}"),
            }
        }
        match tg.send_message(cfg.chat_id, &body).await {
            Ok(id) => ("sent".into(), Some(id)),
            Err(e) => (format!("could not send: {e:#}"), None),
        }
    }

    /// Ask the user something and block until they answer, from either front
    /// end. Returns the answer text, or an explanation of why there isn't one.
    pub async fn ask(&self, session: &str, question: &str, options: &[String]) -> String {
        if options.is_empty() {
            return "need at least one option".into();
        }
        if options.len() > MAX_OPTIONS {
            return format!("too many options — {MAX_OPTIONS} at most");
        }

        let Some(cfg) = self.config().await else {
            return "not set up yet — run `telepager setup`".into();
        };

        let qid = {
            let mut n = self.next_question.lock().await;
            *n += 1;
            format!("q{n}")
        };

        let label = self.session_label(session).await.unwrap_or_else(|| "telepager".into());
        let text = decorate(&label, question);

        // the question is live in the ui whether or not telegram works
        self.registry.lock().await.set_question(session, question, options);

        let tg_message_id = match self.telegram().await {
            Some(tg) => {
                let prompt = format!("{text}\n\n(or reply to this message with your own answer)");
                let buttons: Vec<String> = options.to_vec();
                match tg.send_with_buttons(cfg.chat_id, &prompt, &buttons, &qid).await {
                    Ok(id) => Some(id),
                    Err(e) => {
                        log::warn!("could not send the question to telegram: {e:#}");
                        None
                    }
                }
            }
            None => None,
        };

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            qid.clone(),
            Pending {
                session: session.to_string(),
                question: question.to_string(),
                options: options.to_vec(),
                tg_message_id,
                tx,
            },
        );

        let wait = Duration::from_secs(cfg.ask_timeout_seconds);
        let picked = tokio::time::timeout(wait, rx).await;
        let entry = self.pending.lock().await.remove(&qid);
        let tg_message_id = entry.and_then(|p| p.tg_message_id).or(tg_message_id);

        let (answer, settled) = match picked {
            Ok(Ok(Answer::Tapped(index))) => {
                let answer = options.get(index).cloned().unwrap_or_else(|| "unknown option".into());
                (answer.clone(), format!("{text}\n\n✅ {}. {answer}", index + 1))
            }
            Ok(Ok(Answer::Typed(typed))) => {
                (typed.clone(), format!("{text}\n\n✅ {typed}"))
            }
            Ok(Err(_)) => (
                "no answer — the question was cancelled".into(),
                format!("{text}\n\n⏹ cancelled"),
            ),
            Err(_) => (
                format!("no answer — the user did not pick an option within {}s", cfg.ask_timeout_seconds),
                format!("{text}\n\n⏱ no answer after {}s", cfg.ask_timeout_seconds),
            ),
        };

        self.registry.lock().await.clear_question(session, &answer);

        if let (Some(tg), Some(msg_id)) = (self.telegram().await, tg_message_id) {
            if let Err(e) = tg.edit_message(cfg.chat_id, msg_id, &settled).await {
                log::debug!("could not clear the keyboard: {e:#}");
            }
        }
        answer
    }

    /// Deliver an answer to a waiting question. Returns whether one was found.
    pub async fn answer(&self, qid: &str, answer: Answer) -> bool {
        match self.pending.lock().await.remove(qid) {
            Some(p) => p.tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// Find the question a Telegram message carries the buttons for.
    pub async fn question_for_message(&self, message_id: i64) -> Option<String> {
        self.pending
            .lock()
            .await
            .iter()
            .find(|(_, p)| p.tg_message_id == Some(message_id))
            .map(|(id, _)| id.clone())
    }

    /// Resolve which question a tap meant: the id the button carried if it's
    /// still waiting, otherwise whichever question that message belongs to.
    pub async fn question_for_message_or(&self, qid: &str, message_id: Option<i64>) -> Option<String> {
        if self.pending.lock().await.contains_key(qid) {
            return Some(qid.to_string());
        }
        match message_id {
            Some(id) => self.question_for_message(id).await,
            None => None,
        }
    }

    /// The only pending question, when there's exactly one — what an
    /// unaddressed typed reply in Telegram should answer.
    pub async fn only_pending_question(&self) -> Option<String> {
        let pending = self.pending.lock().await;
        (pending.len() == 1).then(|| pending.keys().next().cloned()).flatten()
    }

    pub async fn pending_questions(&self) -> Vec<Value> {
        self.pending
            .lock()
            .await
            .iter()
            .map(|(id, p)| {
                json!({
                    "id": id,
                    "session": p.session,
                    "question": p.question,
                    "options": p.options,
                })
            })
            .collect()
    }

    // ------------------------------------------------------------ agents

    /// Start an agent and wire its output into the session log.
    pub async fn spawn_agent(
        self: &Arc<Self>,
        agent: &str,
        dir: &str,
        task: &str,
        from_telegram: bool,
    ) -> Result<SessionId> {
        let cfg = self.config().await.context("not set up yet")?;
        let preset = cfg
            .agents
            .get(agent)
            .cloned()
            .with_context(|| format!("no agent called '{agent}' — try one of: {}", cfg.agents.keys().cloned().collect::<Vec<_>>().join(", ")))?;

        if task.trim().is_empty() {
            bail!("the task is empty");
        }

        let path = agents::resolve_dir(dir)?;

        // telegram is the remote surface, so it only reaches allowlisted dirs.
        // the local ui isn't gated: opening it means being at the machine.
        if from_telegram && !agents::dir_is_allowed(&path, &cfg.allowed_dirs) {
            if cfg.allowed_dirs.is_empty() {
                bail!(
                    "spawning from telegram is off until you allow some directories. \
                     add them in the web ui, or set allowed_dirs in {}",
                    config::first_candidate()
                );
            }
            bail!("{} is not in allowed_dirs", path.display());
        }

        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| agent.to_string());

        let id = self
            .open_session(
                Kind::Spawned,
                label,
                Some(path.clone()),
                Some(agent.to_string()),
                Some(task.to_string()),
            )
            .await;

        let launched = match agents::launch(&preset, &path, task) {
            Ok(l) => l,
            Err(e) => {
                let reason = format!("{e:#}");
                self.registry.lock().await.fail(&id, &reason);
                return Err(e);
            }
        };

        self.record(
            Some(&id),
            EventKind::Notice { text: format!("$ {}", launched.rendered.join(" ")) },
        )
        .await;
        self.set_state(&id, State::Working).await;

        let agents::Launched { child, stdout, stderr, stdin, .. } = launched;
        let child = Arc::new(Mutex::new(child));
        self.processes
            .lock()
            .await
            .insert(id.clone(), AgentProcess { child: child.clone(), stdin });

        if let Some(out) = stdout {
            tokio::spawn(pump(self.clone(), id.clone(), "stdout", out));
        }
        if let Some(err) = stderr {
            tokio::spawn(pump(self.clone(), id.clone(), "stderr", err));
        }
        tokio::spawn(reap(self.clone(), id.clone(), child, agent.to_string(), task.to_string()));

        Ok(id)
    }

    /// Type something at a running agent's stdin.
    pub async fn write_to_agent(&self, id: &str, text: &str) -> Result<()> {
        let mut procs = self.processes.lock().await;
        let proc = procs.get_mut(id).context("that session isn't a running agent")?;
        let stdin = proc.stdin.as_mut().context("that agent isn't reading stdin any more")?;

        stdin.write_all(text.as_bytes()).await.context("writing to the agent")?;
        if !text.ends_with('\n') {
            stdin.write_all(b"\n").await.ok();
        }
        stdin.flush().await.ok();
        drop(procs);

        self.record(Some(id), EventKind::Notice { text: format!("< {text}") }).await;
        Ok(())
    }

    /// Close an agent's stdin, which is how most CLIs are told to finish.
    pub async fn close_agent_stdin(&self, id: &str) -> Result<()> {
        let mut procs = self.processes.lock().await;
        let proc = procs.get_mut(id).context("that session isn't a running agent")?;
        proc.stdin = None;
        Ok(())
    }

    pub async fn kill_agent(&self, id: &str) -> Result<()> {
        let child = {
            let procs = self.processes.lock().await;
            let proc = procs.get(id).context("that session isn't a running agent")?;
            proc.child.clone()
        };
        agents::kill_tree(&mut *child.lock().await).await?;
        self.record(Some(id), EventKind::Notice { text: "killed".into() }).await;
        Ok(())
    }

    pub async fn is_running(&self, id: &str) -> bool {
        self.processes.lock().await.contains_key(id)
    }
}

/// Copy one of a child's streams into the session log, line by line.
async fn pump<R>(core: Arc<Core>, id: SessionId, stream: &'static str, reader: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                core.record(
                    Some(&id),
                    EventKind::Output { stream: stream.to_string(), line },
                )
                .await;
            }
            Ok(None) => break,
            Err(e) => {
                log::debug!("{id} {stream} ended: {e:#}");
                break;
            }
        }
    }
}

/// Wait for an agent to finish, then close the session and page the user.
async fn reap(core: Arc<Core>, id: SessionId, child: Arc<Mutex<Child>>, agent: String, task: String) {
    // polled rather than awaited, so `kill_agent` can take the lock meanwhile
    let status = loop {
        match child.lock().await.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    core.processes.lock().await.remove(&id);

    let code = match &status {
        Ok(s) => s.code(),
        Err(e) => {
            log::debug!("waiting on {id} failed: {e:#}");
            None
        }
    };
    core.close_session(&id, code).await;

    let tail = core.output_tail(&id, EXIT_TAIL_LINES).await.unwrap_or_default();
    let outcome = match code {
        Some(0) => "finished".to_string(),
        Some(c) => format!("exited with code {c}"),
        None => "was stopped".to_string(),
    };

    let mut text = format!("🤖 {agent} {outcome}\n\ntask: {task}");
    if !tail.trim().is_empty() {
        text.push_str(&format!("\n\n{tail}"));
    }
    core.send(Some(&id), &text).await;
}

// so you can tell which project is talking when more than one is
pub fn decorate(label: &str, text: &str) -> String {
    format!("📁 {label}\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> Arc<Core> {
        // a path that will never exist, so the core comes up unconfigured
        Core::new(Some(PathBuf::from("/telepager-test/none.json")))
    }

    #[test]
    fn label_goes_above_the_text() {
        assert_eq!(decorate("telepager", "hello"), "📁 telepager\nhello");
    }

    #[tokio::test]
    async fn a_core_with_no_config_still_runs() {
        let c = core();
        assert!(!c.is_configured().await);
        assert!(c.telegram().await.is_none());
        // and it can still track sessions, which is what the ui draws
        let id = c.open_session(Kind::Attached, "x".into(), None, None, None).await;
        assert_eq!(c.sessions().await.len(), 1);
        assert_eq!(c.session_label(&id).await.as_deref(), Some("x"));
    }

    #[tokio::test]
    async fn paging_without_a_config_explains_itself_instead_of_panicking() {
        let c = core();
        let id = c.open_session(Kind::Attached, "x".into(), None, None, None).await;
        assert!(c.send(Some(&id), "hi").await.contains("not set up"));
        let (result, _) = c.thinking(Some(&id), "working", None).await;
        assert!(result.contains("not set up"));
        assert!(c.ask(&id, "q", &["a".into()]).await.contains("not set up"));
    }

    #[tokio::test]
    async fn questions_validate_their_options() {
        let c = core();
        let id = c.open_session(Kind::Attached, "x".into(), None, None, None).await;
        assert!(c.ask(&id, "q", &[]).await.contains("at least one"));
        let too_many: Vec<String> = (0..MAX_OPTIONS + 1).map(|i| i.to_string()).collect();
        assert!(c.ask(&id, "q", &too_many).await.contains("at most"));
    }

    #[tokio::test]
    async fn answering_an_unknown_question_is_false_not_a_panic() {
        let c = core();
        assert!(!c.answer("q999", Answer::Typed("hi".into())).await);
        assert!(c.question_for_message(1234).await.is_none());
        assert!(c.only_pending_question().await.is_none());
    }

    #[tokio::test]
    async fn spawning_without_a_config_is_an_error() {
        let c = core();
        let err = c.spawn_agent("claude", "/tmp", "do it", false).await.unwrap_err();
        assert!(format!("{err:#}").contains("not set up"), "{err:#}");
    }

    #[tokio::test]
    async fn writing_to_a_session_that_is_not_an_agent_is_an_error() {
        let c = core();
        let id = c.open_session(Kind::Attached, "x".into(), None, None, None).await;
        assert!(c.write_to_agent(&id, "hi").await.is_err());
        assert!(c.kill_agent(&id).await.is_err());
        assert!(!c.is_running(&id).await);
    }

    #[tokio::test]
    async fn sessions_are_pruned_but_live_ones_survive() {
        let c = core();
        for _ in 0..(KEEP_FINISHED + 10) {
            let id = c.open_session(Kind::Spawned, "gone".into(), None, None, None).await;
            c.close_session(&id, Some(0)).await;
        }
        let live = c.open_session(Kind::Attached, "live".into(), None, None, None).await;
        assert!(c.sessions().await.len() <= KEEP_FINISHED + 1);
        assert_eq!(c.live_count().await, 1);
        assert!(c.session_label(&live).await.is_some());
    }

    #[tokio::test]
    async fn the_ui_url_carries_the_key() {
        let c = core();
        assert!(c.ui().await.is_none());
        c.set_ui(UiInfo { port: 4321, key: "abc".into() }).await;
        let url = c.ui().await.unwrap().url();
        assert_eq!(url, "http://127.0.0.1:4321/?k=abc");
    }

    #[tokio::test]
    async fn events_reach_subscribers() {
        let c = core();
        let mut rx = c.subscribe().await;
        c.notice("something happened").await;
        let event = rx.recv().await.unwrap();
        assert!(matches!(event.kind, EventKind::Notice { .. }));
    }
}
