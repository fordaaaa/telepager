//! The one thing that owns everything: telegram, sessions, spawned processes,
//! the master's conversation. ipc, web and the telegram poller are all clients
//! of it, so routing a question or starting an agent happens in one place.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portable_pty::{MasterPty, PtySize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use crate::agents::{self, Handle};
use crate::config::{self, Config};
use crate::screen::{self, Screen};
use crate::session::{Event, EventKind, Kind, Registry, SessionId, State};
use crate::telegram::Telegram;

/// Cap on options in a question — Telegram keyboards get unusable past this.
pub const MAX_OPTIONS: usize = 20;
/// Finished sessions kept for scrollback before they're dropped.
const KEEP_FINISHED: usize = 30;
/// Output lines quoted back to Telegram when an agent finishes.
const EXIT_TAIL_LINES: usize = 15;
/// How much of a shell command's output is quoted back. It's all in the session
/// log either way; this just caps what lands on someone's phone.
const SHELL_TAIL_LINES: usize = 40;
const SHELL_TAIL_CHARS: usize = 3000;
/// How often a busy screen may say it changed — a tui redraws faster than
/// anyone reads, and the bus is shared.
const SCREEN_TICK: Duration = Duration::from_millis(80);

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
    /// The messages carrying the buttons, one per chat, so a tap from any of
    /// them finds the question.
    tg_messages: Fanout,
    tx: oneshot::Sender<Answer>,
}

/// Where one outbound message landed: a message id per chat. Everything goes to
/// every allowed user, so a status line being edited is several messages.
#[derive(Debug, Clone, Default)]
pub struct Fanout {
    sent: Vec<(i64, i64)>,
}

impl Fanout {
    fn message_in(&self, chat: i64) -> Option<i64> {
        self.sent.iter().find(|(c, _)| *c == chat).map(|(_, m)| *m)
    }

    fn carries(&self, message_id: i64) -> bool {
        self.sent.iter().any(|(_, m)| *m == message_id)
    }

    pub fn is_empty(&self) -> bool {
        self.sent.is_empty()
    }

    /// How many chats it actually reached.
    pub fn len(&self) -> usize {
        self.sent.len()
    }
}

/// A running agent process, kept so it can be written to and killed. The child
/// has its own lock so the reaper can poll it without holding the whole table.
struct AgentProcess {
    child: Arc<Mutex<Handle>>,
    /// Own lock: writing to an agent that stopped reading must not wedge the
    /// whole process table while the pipe buffer is full.
    input: AgentInput,
    /// The pty master, for resizes.
    pty: Option<Arc<std::sync::Mutex<Box<dyn MasterPty + Send>>>>,
}

/// Where a follow-up typed at an agent goes.
#[derive(Clone)]
enum AgentInput {
    Pipe(Arc<Mutex<Option<ChildStdin>>>),
    /// A pty write blocks, so it happens on a blocking thread.
    Pty(Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>>),
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
    /// None until telegram is set up — running unconfigured is what lets us
    /// serve the setup page.
    cfg: RwLock<Option<Config>>,
    tg: RwLock<Option<Arc<Telegram>>>,
    registry: Mutex<Registry>,
    pending: Mutex<HashMap<String, Pending>>,
    processes: Mutex<HashMap<SessionId, AgentProcess>>,
    /// Bumped per question so ids are unique for the lifetime of the process.
    next_question: Mutex<u64>,
    /// The master agent's conversation, if it's been used.
    pub master: Mutex<crate::master::Conversation>,
    /// Held for a whole master turn. Telegram runs a task per message, and two
    /// turns pushing into one history interleave into an ordering no provider
    /// will accept back.
    pub master_turn: Mutex<()>,
    /// Woken when the config changes, so the Telegram poller restarts.
    pub config_changed: Notify,
    ui: RwLock<Option<UiInfo>>,
    /// Where `/sh` runs, set by `/cd`. Here rather than a global so tests get
    /// their own and it dies with the daemon.
    work_dir: RwLock<Option<PathBuf>>,
}

/// What a shell command did.
pub struct ShellRun {
    pub session: SessionId,
    /// One line: how it ended.
    pub outcome: String,
    /// The tail of what it printed, capped.
    pub output: String,
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
            master_turn: Mutex::new(()),
            config_changed: Notify::new(),
            ui: RwLock::new(None),
            work_dir: RwLock::new(None),
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

    /// Re-read the config and swap in a fresh telegram client, so the setup
    /// page takes effect without a restart.
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

    /// Where `/sh` runs. Its caller has already checked the allowlist.
    pub async fn set_work_dir(&self, dir: PathBuf) {
        *self.work_dir.write().await = Some(dir);
    }

    pub async fn work_dir(&self) -> Option<PathBuf> {
        self.work_dir.read().await.clone()
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

    // -------------------------------------------------------- broadcasting

    /// Send one message to every allowed user. One chat failing — blocked the
    /// bot, never said hello — must not stop the others, so it's logged and the
    /// rest still go out.
    pub async fn broadcast(&self, text: &str) -> (String, Fanout) {
        let (Some(tg), Some(cfg)) = (self.telegram().await, self.config().await) else {
            return ("not set up yet — run `telepager setup`".into(), Fanout::default());
        };

        let mut out = Fanout::default();
        let mut last_error = None;
        for chat in cfg.chat_ids() {
            match tg.send_message(chat, text).await {
                Ok(id) => out.sent.push((chat, id)),
                Err(e) => {
                    log::warn!("could not page {chat}: {e:#}");
                    last_error = Some(format!("{e:#}"));
                }
            }
        }

        if out.is_empty() {
            let why = last_error.unwrap_or_else(|| "nobody to send to".into());
            return (format!("could not send: {why}"), out);
        }
        ("sent".into(), out)
    }

    /// Edit a message already broadcast, in every chat it reached. A chat that
    /// hasn't got one yet — or whose edit fails — gets a fresh message.
    pub async fn broadcast_edit(&self, previous: &Fanout, text: &str) -> (String, Fanout) {
        let (Some(tg), Some(cfg)) = (self.telegram().await, self.config().await) else {
            return ("not set up yet — run `telepager setup`".into(), previous.clone());
        };

        let mut out = Fanout::default();
        let mut last_error = None;
        for chat in cfg.chat_ids() {
            if let Some(id) = previous.message_in(chat) {
                match tg.edit_message(chat, id, text).await {
                    Ok(()) => {
                        out.sent.push((chat, id));
                        continue;
                    }
                    Err(e) => log::debug!("status edit failed, sending a new one: {e:#}"),
                }
            }
            match tg.send_message(chat, text).await {
                Ok(id) => out.sent.push((chat, id)),
                Err(e) => {
                    log::warn!("could not page {chat}: {e:#}");
                    last_error = Some(format!("{e:#}"));
                }
            }
        }

        if out.is_empty() {
            let why = last_error.unwrap_or_else(|| "nobody to send to".into());
            return (format!("could not send: {why}"), out);
        }
        ("sent".into(), out)
    }

    /// Take a broadcast message back down — the master's status line is just
    /// noise once the answer arrives.
    pub async fn erase(&self, sent: &Fanout) {
        let Some(tg) = self.telegram().await else { return };
        for (chat, id) in &sent.sent {
            if let Err(e) = tg.delete_message(*chat, *id).await {
                log::debug!("could not take the status line down: {e:#}");
            }
        }
    }

    /// Take one message back down, wherever it came from. A key someone typed
    /// shouldn't stay in the chat history.
    pub async fn unsend(&self, chat: i64, message_id: i64) -> bool {
        let Some(tg) = self.telegram().await else { return false };
        match tg.delete_message(chat, message_id).await {
            Ok(()) => true,
            Err(e) => {
                log::debug!("could not delete a message: {e:#}");
                false
            }
        }
    }

    /// Tell everyone something without making the caller wait on Telegram.
    pub fn announce(self: &Arc<Self>, text: String) {
        let core = self.clone();
        tokio::spawn(async move {
            core.broadcast(&text).await;
        });
    }

    // ----------------------------------------------------------- paging

    /// Send a message to Telegram on a session's behalf.
    pub async fn send(&self, session: Option<&str>, text: &str) -> String {
        let label = match session {
            Some(id) => self.session_label(id).await.unwrap_or_else(|| "master agent".into()),
            None => "master agent".into(),
        };

        if let Some(id) = session {
            self.registry
                .lock()
                .await
                .record(Some(id), EventKind::Message { text: text.to_string() });
        }

        self.broadcast(&decorate(&label, text)).await.0
    }

    /// The in-place status line. `previous` is the broadcast being edited.
    pub async fn thinking(&self, session: Option<&str>, text: &str, previous: &Fanout) -> (String, Fanout) {
        let label = match session {
            Some(id) => self.session_label(id).await.unwrap_or_else(|| "master agent".into()),
            None => "master agent".into(),
        };
        if let Some(id) = session {
            self.registry.lock().await.set_thinking(id, text);
        }

        self.broadcast_edit(previous, &decorate(&label, &format!("💭 {text}"))).await
    }

    /// Ask the user something and block until either front end answers. Returns
    /// the answer, or why there isn't one.
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

        let label = self.session_label(session).await.unwrap_or_else(|| "master agent".into());
        let text = decorate(&label, question);

        // the question is live in the ui whether or not telegram works
        self.registry.lock().await.set_question(session, question, options);

        let mut tg_messages = Fanout::default();
        if let Some(tg) = self.telegram().await {
            let prompt = format!("{text}\n\n(or reply to this message with your own answer)");
            for chat in cfg.chat_ids() {
                match tg.send_with_buttons(chat, &prompt, options, &qid).await {
                    Ok(id) => tg_messages.sent.push((chat, id)),
                    Err(e) => log::warn!("could not send the question to {chat}: {e:#}"),
                }
            }
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            qid.clone(),
            Pending {
                session: session.to_string(),
                question: question.to_string(),
                options: options.to_vec(),
                tg_messages: tg_messages.clone(),
                tx,
            },
        );

        let wait = Duration::from_secs(cfg.ask_timeout_seconds);
        let picked = tokio::time::timeout(wait, rx).await;
        let entry = self.pending.lock().await.remove(&qid);
        let tg_messages = entry.map(|p| p.tg_messages).unwrap_or(tg_messages);

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

        // every chat got buttons, so every chat's keyboard has to go
        if let Some(tg) = self.telegram().await {
            for (chat, msg_id) in &tg_messages.sent {
                if let Err(e) = tg.edit_message(*chat, *msg_id, &settled).await {
                    log::debug!("could not clear the keyboard in {chat}: {e:#}");
                }
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

    /// Find the question a Telegram message carries the buttons for. The tap
    /// or reply can come from any of the chats the question went to.
    pub async fn question_for_message(&self, message_id: i64) -> Option<String> {
        self.pending
            .lock()
            .await
            .iter()
            .find(|(_, p)| p.tg_messages.carries(message_id))
            .map(|(id, _)| id.clone())
    }

    /// Which question a tap meant: the id on the button if it's still waiting,
    /// else whichever question that message belongs to.
    pub async fn question_for_message_or(&self, qid: &str, message_id: Option<i64>) -> Option<String> {
        if self.pending.lock().await.contains_key(qid) {
            return Some(qid.to_string());
        }
        match message_id {
            Some(id) => self.question_for_message(id).await,
            None => None,
        }
    }

    /// The only pending question, if there's exactly one — what a typed reply
    /// might be answering.
    pub async fn only_pending_question(&self) -> Option<(String, Vec<String>)> {
        let pending = self.pending.lock().await;
        if pending.len() != 1 {
            return None;
        }
        pending.iter().next().map(|(id, p)| (id.clone(), p.options.clone()))
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
        model: Option<&str>,
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

        let launched = match agents::launch(&preset, &path, task, model) {
            Ok(l) => l,
            Err(e) => {
                let reason = format!("{e:#}");
                self.registry.lock().await.fail(&id, &reason);
                self.announce(format!("⚠️ {agent} wouldn't start in {}: {reason}", path.display()));
                return Err(e);
            }
        };

        // whoever started it, everyone hears about it
        self.announce(format!("🚀 {agent} started in {} as {id}\n\ntask: {task}", path.display()));

        self.record(
            Some(&id),
            EventKind::Notice { text: format!("$ {}", launched.rendered.join(" ")) },
        )
        .await;
        self.set_state(&id, State::Working).await;

        let agents::Launched { child, io, .. } = launched;
        let child = Arc::new(Mutex::new(child));

        let (input, pty) = match io {
            agents::Io::Pipes { stdout, stderr, stdin } => {
                if let Some(out) = stdout {
                    tokio::spawn(pump(self.clone(), id.clone(), "stdout", out));
                }
                if let Some(err) = stderr {
                    tokio::spawn(pump(self.clone(), id.clone(), "stderr", err));
                }
                (AgentInput::Pipe(Arc::new(Mutex::new(stdin))), None)
            }
            agents::Io::Pty { master, reader, writer } => {
                let screen = Arc::new(std::sync::Mutex::new(Screen::new(
                    screen::DEFAULT_COLS,
                    screen::DEFAULT_ROWS,
                )));
                self.registry.lock().await.attach_screen(&id, screen.clone());
                tokio::spawn(pump_pty(self.clone(), id.clone(), screen, reader));
                (
                    AgentInput::Pty(Arc::new(std::sync::Mutex::new(Some(writer)))),
                    Some(Arc::new(std::sync::Mutex::new(master))),
                )
            }
        };

        self.processes
            .lock()
            .await
            .insert(id.clone(), AgentProcess { child: child.clone(), input, pty });

        tokio::spawn(reap(self.clone(), id.clone(), child, agent.to_string(), task.to_string()));

        Ok(id)
    }

    /// Run a shell command as a session, so it shows up in `list_sessions`, the
    /// console's terminal pane, and `/kill`.
    ///
    /// This is the mechanism, not the gate — the caller decides whether it may
    /// run. `confirm` is asked first; a "no" means it never starts.
    pub async fn run_shell(
        self: &Arc<Self>,
        command: &str,
        dir: &Path,
        timeout: Duration,
        confirm: Option<&str>,
    ) -> Result<ShellRun> {
        if command.trim().is_empty() {
            bail!("the command is empty");
        }

        let label = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".into());
        let id = self
            .open_session(
                Kind::Spawned,
                label,
                Some(dir.to_path_buf()),
                Some("shell".into()),
                Some(command.to_string()),
            )
            .await;

        self.record(Some(&id), EventKind::Notice { text: format!("$ {command}") }).await;
        // whichever surface ran it, everyone sees it
        self.announce(format!("🖥 shell in {} as {id}\n\n$ {command}", dir.display()));

        if let Some(question) = confirm {
            let answer = self.ask(&id, question, &["yes".into(), "no".into()]).await;
            if !answer.trim().eq_ignore_ascii_case("yes") {
                self.record(Some(&id), EventKind::Notice { text: "not run".into() }).await;
                self.close_session(&id, None).await;
                return Ok(ShellRun {
                    session: id,
                    outcome: format!("not run — you said: {answer}"),
                    output: String::new(),
                });
            }
        }

        self.set_state(&id, State::Working).await;

        let mut cmd = shell_command(command);
        cmd.current_dir(dir)
            // nothing to type at it, and an inherited stdin would be the
            // daemon's terminal
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .env("TERM", "dumb");
        // its own group, like every other child: so the timeout can take the
        // whole tree, and so a signal it sends can't reach the daemon
        agents::own_process_group(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let reason = format!("{e}");
                self.registry.lock().await.fail(&id, &reason);
                return Err(e).context("starting a shell");
            }
        };

        let pumps = (
            child.stdout.take().map(|out| tokio::spawn(pump(self.clone(), id.clone(), "stdout", out))),
            child.stderr.take().map(|err| tokio::spawn(pump(self.clone(), id.clone(), "stderr", err))),
        );

        let child = Arc::new(Mutex::new(Handle::Piped(child)));
        self.processes.lock().await.insert(
            id.clone(),
            AgentProcess {
                child: child.clone(),
                // stdin is closed, so there's nothing to type at
                input: AgentInput::Pipe(Arc::new(Mutex::new(None))),
                pty: None,
            },
        );

        let finish = async {
            if let Some(h) = pumps.0 {
                let _ = h.await;
            }
            if let Some(h) = pumps.1 {
                let _ = h.await;
            }
            // polled, not awaited, so kill_agent can take the lock meanwhile
            loop {
                match child.lock().await.try_wait() {
                    Ok(Some(exit)) => return exit.code,
                    Ok(None) => {}
                    Err(e) => {
                        log::debug!("waiting on a shell command failed: {e:#}");
                        return None;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };

        let mut run = std::pin::pin!(finish);
        let (code, outcome) = tokio::select! {
            code = &mut run => (code, match code {
                Some(0) => "exit 0".to_string(),
                Some(c) => format!("exit {c}"),
                None => "stopped".to_string(),
            }),
            _ = tokio::time::sleep(timeout) => {
                // the whole group, or a build carries on with nobody reading it
                if let Err(e) = agents::kill_tree(&mut *child.lock().await).await {
                    log::debug!("could not kill {id}: {e:#}");
                }
                // let the pumps hand over what it managed to print
                let _ = tokio::time::timeout(Duration::from_secs(2), run).await;
                (None, format!("timed out after {}s, killed", timeout.as_secs()))
            }
        };

        self.processes.lock().await.remove(&id);
        self.record(Some(&id), EventKind::Notice { text: outcome.clone() }).await;
        self.close_session(&id, code).await;

        let output = self.output_tail(&id, SHELL_TAIL_LINES).await.unwrap_or_default();
        Ok(ShellRun { session: id, outcome, output: tail_chars(&output, SHELL_TAIL_CHARS) })
    }

    /// A pty session's screen: all of it, or what changed since `since`.
    pub async fn screen_view(&self, id: &str, since: Option<u64>) -> Result<Value> {
        let screen = self
            .registry
            .lock()
            .await
            .screen(id)
            .context("that session isn't running on a terminal")?;
        let view = screen::lock(&screen).view(since);
        Ok(view)
    }

    /// Tell a session's screen — and the agent drawing on it — a new size.
    pub async fn resize_session(&self, id: &str, cols: u16, rows: u16) -> Result<Value> {
        let screen = self
            .registry
            .lock()
            .await
            .screen(id)
            .context("that session isn't running on a terminal")?;

        let (revision, cols, rows) = {
            let mut screen = screen::lock(&screen);
            screen.resize(cols, rows);
            (screen.revision(), screen.cols(), screen.rows())
        };

        // the kernel has to agree, or the agent keeps drawing the old size
        if let Some(pty) = self.processes.lock().await.get(id).and_then(|p| p.pty.clone()) {
            let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
            let result = match pty.lock() {
                Ok(m) => m.resize(size),
                Err(p) => p.into_inner().resize(size),
            };
            if let Err(e) = result {
                log::debug!("could not resize {id}'s pty: {e:#}");
            }
        }

        self.record(Some(id), EventKind::Screen { revision }).await;
        Ok(json!({ "revision": revision, "cols": cols, "rows": rows }))
    }

    /// Type something at a running agent's stdin.
    pub async fn write_to_agent(&self, id: &str, text: &str) -> Result<()> {
        // take the handle and let go of the table: an agent that has stopped
        // reading stdin will block this write until its pipe drains, and
        // holding the table meanwhile would wedge every other agent too
        let input = {
            let procs = self.processes.lock().await;
            let proc = procs.get(id).context("that session isn't a running agent")?;
            proc.input.clone()
        };

        match input {
            AgentInput::Pipe(stdin) => {
                let mut guard = stdin.lock().await;
                let handle = guard
                    .as_mut()
                    .context("that agent isn't reading stdin any more")?;

                handle.write_all(text.as_bytes()).await.context("writing to the agent")?;
                if !text.ends_with('\n') {
                    handle.write_all(b"\n").await.ok();
                }
                handle.flush().await.ok();
            }
            // a terminal sees the return key as CR, not LF
            AgentInput::Pty(writer) => {
                let mut bytes = text.as_bytes().to_vec();
                if !text.ends_with('\r') && !text.ends_with('\n') {
                    bytes.push(b'\r');
                }
                pty_write(writer, bytes).await?;
            }
        }

        self.record(Some(id), EventKind::Notice { text: format!("< {text}") }).await;
        Ok(())
    }

    /// Close an agent's stdin, which is how most CLIs are told to finish.
    pub async fn close_agent_stdin(&self, id: &str) -> Result<()> {
        let input = {
            let procs = self.processes.lock().await;
            let proc = procs.get(id).context("that session isn't a running agent")?;
            proc.input.clone()
        };
        match input {
            // dropping the handle is what actually sends EOF
            AgentInput::Pipe(stdin) => *stdin.lock().await = None,
            // a pty never closes; ^D is how you say end-of-input to one
            AgentInput::Pty(writer) => pty_write(writer, vec![0x04]).await?,
        }
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

    /// Wire a core straight to a config and client, so a test needs neither a
    /// config file nor the real api.
    #[cfg(test)]
    pub async fn attach(&self, cfg: Config, tg: Telegram) {
        *self.cfg.write().await = Some(cfg);
        *self.tg.write().await = Some(Arc::new(tg));
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

/// A pty write blocks, so it happens off the runtime.
async fn pty_write(
    writer: Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>>,
    bytes: Vec<u8>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
        let handle = guard.as_mut().context("that agent isn't reading input any more")?;
        handle.write_all(&bytes).context("writing to the agent")?;
        handle.flush().ok();
        Ok(())
    })
    .await
    .context("writing to the agent")?
}

/// Feed a pty into a session's screen. The read blocks, so it gets its own
/// thread. Screen events are coalesced or a redrawing tui puts a revision per
/// frame on the shared bus.
async fn pump_pty(core: Arc<Core>, id: SessionId, screen: screen::Shared, mut reader: Box<dyn std::io::Read + Send>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            // a closed pty reads as an error, not eof, on some platforms
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut announced = 0u64;
    let mut last = Instant::now();

    loop {
        let chunk = tokio::time::timeout(SCREEN_TICK, rx.recv()).await;
        let revision = match chunk {
            Ok(Some(bytes)) => {
                let (revision, lines) = {
                    let mut screen = screen::lock(&screen);
                    screen.feed(&bytes);
                    (screen.revision(), screen.take_lines())
                };
                // what scrolled off is the session's output log, which
                // read_output and the telegram summaries still read
                for line in lines {
                    core.record(Some(&id), EventKind::Output { stream: "screen".into(), line })
                        .await;
                }
                revision
            }
            Ok(None) => break,
            Err(_) => screen::lock(&screen).revision(),
        };

        if revision != announced && last.elapsed() >= SCREEN_TICK {
            announced = revision;
            last = Instant::now();
            core.record(Some(&id), EventKind::Screen { revision }).await;
        }
    }

    let revision = screen::lock(&screen).revision();
    if revision != announced {
        core.record(Some(&id), EventKind::Screen { revision }).await;
    }
}

/// Wait for an agent to finish, then close the session and page the user.
async fn reap(core: Arc<Core>, id: SessionId, child: Arc<Mutex<Handle>>, agent: String, task: String) {
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
        Ok(s) => s.code,
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

/// The command that runs a command line, per platform.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

/// The last `limit` characters, on a line boundary where there is one.
fn tail_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().skip(text.chars().count() - limit).collect();
    let kept = match kept.find('\n') {
        Some(i) => &kept[i + 1..],
        None => kept.as_str(),
    };
    format!("…\n{kept}")
}

// so you can tell which project is talking when more than one is
pub fn decorate(label: &str, text: &str) -> String {
    format!("📁 {label}\n{text}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// Chat ids a fake Telegram was asked to send to, in order.
    pub(crate) type Seen = Arc<std::sync::Mutex<Vec<i64>>>;

    /// A stand-in telegram api on localhost: accepts `good`, refuses every
    /// other chat — that's how a test gets a chat that has blocked the bot.
    pub(crate) async fn fake_telegram(good: i64) -> (String, Seen, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));

        let recorder = seen.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                // the body is what we're after, so read until it shows up
                while let Ok(n) = stream.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..n]);
                    if String::from_utf8_lossy(&request).contains("chat_id") {
                        break;
                    }
                }

                let chat = chat_id_in(&String::from_utf8_lossy(&request));
                if let Some(chat) = chat {
                    recorder.lock().unwrap().push(chat);
                }
                let body = if chat == Some(good) {
                    r#"{"ok":true,"result":{"message_id":11}}"#
                } else {
                    r#"{"ok":false,"description":"chat not found"}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await.ok();
            }
        });

        (base, seen, server)
    }

    fn chat_id_in(request: &str) -> Option<i64> {
        let rest = request.split("\"chat_id\":").nth(1)?;
        let digits: String = rest
            .chars()
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        digits.parse().ok()
    }

    pub(crate) fn config_for(ids: &[i64]) -> Config {
        Config {
            token: "1:fake".into(),
            allowed_user_ids: ids.to_vec(),
            chat_id: ids[0],
            ask_timeout_seconds: 5,
            master: Default::default(),
            agents: Default::default(),
            allowed_dirs: Vec::new(),
            ui_port: 0,
            permissions: Default::default(),
        }
    }

    /// A core talking to a fake Telegram that only `ids[0]` can receive from.
    pub(crate) async fn wired_core(ids: &[i64]) -> (Arc<Core>, Seen, tokio::task::JoinHandle<()>) {
        let (base, seen, server) = fake_telegram(ids[0]).await;
        let c = core();
        c.attach(config_for(ids), Telegram::with_base(&base)).await;
        (c, seen, server)
    }

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
        let (result, _) = c.thinking(Some(&id), "working", &Fanout::default()).await;
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
        let err = c.spawn_agent("claude", "/tmp", "do it", None, false).await.unwrap_err();
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

    /// A core with a real config file, so `spawn_agent` works. "sleeper" never
    /// reads its stdin — the shape that used to wedge the process table.
    fn configured_core(name: &str) -> (Arc<Core>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("telepager-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "bot_token": "1:fake",
                "allowed_user_ids": [1],
                "agents": {
                    "sleeper": { "command": "sh", "args": ["-c", "sleep 30"] }
                }
            })
            .to_string(),
        )
        .unwrap();
        (Core::new(Some(path.clone())), dir)
    }

    #[tokio::test]
    async fn a_blocked_stdin_write_does_not_wedge_the_process_table() {
        if crate::agents::which("sh").is_none() {
            return;
        }
        let (core, dir) = configured_core("stdin");
        let id = core
            .spawn_agent("sleeper", dir.to_str().unwrap(), "sleep", None, false)
            .await
            .unwrap();

        // more than a pipe buffer, aimed at an agent that never reads: this
        // write cannot complete, and must not be holding the table
        let writer = {
            let core = core.clone();
            let id = id.clone();
            tokio::spawn(async move { core.write_to_agent(&id, &"x".repeat(1024 * 1024)).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;

        // the table has to still be usable while that write is stuck
        let reachable = tokio::time::timeout(Duration::from_secs(5), core.is_running(&id)).await;
        assert!(reachable.is_ok(), "the process table was blocked by a stuck write");

        let killed = tokio::time::timeout(Duration::from_secs(10), core.kill_agent(&id)).await;
        assert!(killed.is_ok(), "kill_agent could not run while a write was stuck");

        writer.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A core whose only agent keeps appending to a file: a killed process is
    /// still a pid until it's reaped, but it stops ticking.
    #[cfg(unix)]
    fn ticking_core(name: &str) -> (Arc<Core>, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("telepager-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ticks");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "bot_token": "1:fake",
                "allowed_user_ids": [1],
                "agents": {
                    "ticker": {
                        "command": "sh",
                        // bounded, so a test that fails still can't leave a
                        // process behind for long
                        "args": [
                            "-c",
                            format!(
                                "i=0; while [ $i -lt 15 ]; do echo $i >> {}; i=$((i+1)); sleep 1; done",
                                marker.display()
                            ),
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        (Core::new(Some(path)), dir, marker)
    }

    // regression: agents were launched with kill_on_drop(true), so the process
    // table owned their lives. anything that dropped the core — the daemon
    // unwinding out of an unrelated error, a runtime shutting down — SIGKILLed
    // every agent that was still working, silently.
    #[cfg(unix)]
    #[test]
    fn shutting_the_runtime_down_does_not_kill_running_agents() {
        if crate::agents::which("sh").is_none() {
            return;
        }
        let (core, dir, marker) = ticking_core("drop");
        let ticks = || std::fs::read_to_string(&marker).map(|t| t.lines().count()).unwrap_or(0);

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            core.spawn_agent("ticker", dir.to_str().unwrap(), "tick", None, false)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1200)).await;
        });
        let before = ticks();
        assert!(before > 0, "the agent never started");

        // this is the shape the daemon takes when it unwinds: every task goes,
        // and with them the last references to the core and its process table
        drop(core);
        drop(runtime);

        std::thread::sleep(Duration::from_millis(2500));
        assert!(ticks() > before, "shutting the runtime down killed a live agent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A core whose only agent is interactive, so it lands on a pty.
    #[cfg(unix)]
    fn tui_core(name: &str) -> (Arc<Core>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("telepager-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "bot_token": "1:fake",
                "allowed_user_ids": [1],
                "agents": {
                    "tui": {
                        "command": "sh",
                        "args": ["-c", "printf 'hello\\r\\n'; sleep 20"],
                        "interactive": true,
                        "pty": true
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        (Core::new(Some(path)), dir)
    }

    /// Wait for the screen to say something instead of sleeping and hoping.
    #[cfg(unix)]
    async fn screen_says(core: &Arc<Core>, id: &str, want: &str) -> bool {
        for _ in 0..50 {
            if core.output_tail(id, 40).await.unwrap_or_default().contains(want) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pty_agent_gets_a_screen_that_can_be_read_and_resized() {
        if crate::agents::which("sh").is_none() {
            return;
        }
        let (core, dir) = tui_core("screen");
        let id = core.spawn_agent("tui", dir.to_str().unwrap(), "draw", None, false).await.unwrap();

        assert!(screen_says(&core, &id, "hello").await, "the screen never drew");

        let snap = core.screen_view(&id, None).await.unwrap();
        assert_eq!(snap["full"], true);
        assert_eq!(snap["cols"], 80);
        assert_eq!(snap["rows"], 24);
        let revision = snap["revision"].as_u64().unwrap();

        // a diff from where we are is empty, and says the same revision
        let diff = core.screen_view(&id, Some(revision)).await.unwrap();
        assert_eq!(diff["full"], false);
        assert_eq!(diff["revision"], revision);
        assert_eq!(diff["lines"].as_array().unwrap().len(), 0);

        let resized = core.resize_session(&id, 100, 30).await.unwrap();
        assert_eq!(resized["cols"], 100);
        assert_eq!(resized["rows"], 30);
        assert_eq!(core.screen_view(&id, None).await.unwrap()["cols"], 100);

        core.kill_agent(&id).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_session_with_no_screen_says_so_instead_of_panicking() {
        let c = core();
        let id = c.open_session(Kind::Attached, "x".into(), None, None, None).await;
        assert!(c.screen_view(&id, None).await.is_err());
        assert!(c.resize_session(&id, 80, 24).await.is_err());
    }

    #[tokio::test]
    async fn a_broadcast_goes_to_every_allowed_user() {
        let (c, seen, server) = wired_core(&[7, 8, 9]).await;
        let (status, sent) = c.broadcast("hello").await;

        assert_eq!(status, "sent");
        assert_eq!(*seen.lock().unwrap(), vec![7, 8, 9]);
        // only 7 accepted it, so only 7's message can be edited later
        assert_eq!(sent.sent, vec![(7, 11)]);
        server.abort();
    }

    // one person blocking the bot used to be the whole fan-out's problem
    #[tokio::test]
    async fn one_chat_refusing_does_not_stop_the_others() {
        // only 9 can receive, and it's last in line behind two refusals
        let (base, seen, server) = fake_telegram(9).await;
        let c = core();
        c.attach(config_for(&[8, 7, 9]), Telegram::with_base(&base)).await;

        let (status, sent) = c.broadcast("hello").await;
        assert_eq!(*seen.lock().unwrap(), vec![8, 7, 9]);
        assert_eq!(status, "sent");
        assert_eq!(sent.sent, vec![(9, 11)]);
        server.abort();
    }

    #[tokio::test]
    async fn nobody_reachable_is_reported_rather_than_claimed_sent() {
        let (base, _seen, server) = fake_telegram(0).await;
        let c = core();
        c.attach(config_for(&[7, 8]), Telegram::with_base(&base)).await;

        let (status, sent) = c.broadcast("hello").await;
        assert!(status.starts_with("could not send"), "{status}");
        assert!(sent.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn a_status_line_is_edited_where_it_landed_and_sent_where_it_did_not() {
        let (c, seen, server) = wired_core(&[7, 8]).await;
        let (_, first) = c.thinking(None, "working", &Fanout::default()).await;
        assert_eq!(first.sent, vec![(7, 11)]);

        let (_, second) = c.thinking(None, "still working", &first).await;
        assert_eq!(second.sent, vec![(7, 11)]);
        // 7 got an edit, 8 another attempt at a fresh message
        assert_eq!(*seen.lock().unwrap(), vec![7, 8, 7, 8]);
        server.abort();
    }

    #[tokio::test]
    async fn a_tap_from_any_chat_finds_its_question() {
        let (c, _seen, server) = wired_core(&[7]).await;
        let id = c.open_session(Kind::Attached, "proj".into(), None, None, None).await;

        let asker = {
            let c = c.clone();
            let id = id.clone();
            tokio::spawn(async move { c.ask(&id, "ship it?", &["yes".into(), "no".into()]).await })
        };
        let qid = waiting_question(&c).await;

        assert_eq!(c.question_for_message(11).await.as_deref(), Some(qid.as_str()));
        assert!(c.question_for_message(12).await.is_none());
        assert!(c.answer(&qid, Answer::Tapped(1)).await);
        assert_eq!(asker.await.unwrap(), "no");
        server.abort();
    }

    /// Wait for `ask` to park its question, rather than sleeping and hoping.
    pub(crate) async fn waiting_question(core: &Arc<Core>) -> String {
        for _ in 0..100 {
            if let Some((qid, _)) = core.only_pending_question().await {
                return qid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no question was ever waiting");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_command_becomes_a_session_with_its_output_and_exit_code() {
        if crate::agents::which("sh").is_none() {
            return;
        }
        let c = core();
        let run = c
            .run_shell("echo hello; exit 3", &std::env::temp_dir(), Duration::from_secs(10), None)
            .await
            .unwrap();

        assert_eq!(run.outcome, "exit 3");
        assert!(run.output.contains("hello"), "{}", run.output);
        // it's a real session, so the console and list_sessions can see it
        assert!(c.session_lines().await.iter().any(|l| l.contains(&run.session)));
        assert!(!c.is_running(&run.session).await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_is_not_confirmed_never_runs() {
        if crate::agents::which("sh").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("telepager-confirm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");

        // an unconfigured core can't ask anyone, which is not a yes
        let c = core();
        let run = c
            .run_shell(
                &format!("touch {}", marker.display()),
                &dir,
                Duration::from_secs(10),
                Some("really?"),
            )
            .await
            .unwrap();

        assert!(run.outcome.starts_with("not run"), "{}", run.outcome);
        assert!(!marker.exists(), "the command ran without a yes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confirming_lets_it_run() {
        if crate::agents::which("sh").is_none() {
            return;
        }
        let (c, _seen, server) = wired_core(&[7]).await;
        let runner = {
            let c = c.clone();
            tokio::spawn(async move {
                c.run_shell("echo yes", &std::env::temp_dir(), Duration::from_secs(10), Some("really?"))
                    .await
                    .unwrap()
            })
        };

        let qid = waiting_question(&c).await;
        assert!(c.answer(&qid, Answer::Tapped(0)).await);

        let run = runner.await.unwrap();
        assert_eq!(run.outcome, "exit 0");
        server.abort();
    }

    // the timeout has to take the whole group: the direct child here is an `sh`
    // sitting in a sleep, and the thing still writing is a subshell it
    // backgrounded. killing only the child leaves that ticking away.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_takes_the_whole_process_group() {
        if crate::agents::which("sh").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("telepager-shtimeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ticks");
        let ticks = || std::fs::read_to_string(&marker).map(|t| t.lines().count()).unwrap_or(0);

        let c = core();
        let command = format!(
            "(i=0; while [ $i -lt 30 ]; do echo $i >> {}; i=$((i+1)); sleep 1; done) & sleep 30",
            marker.display()
        );
        let run = c
            .run_shell(&command, &dir, Duration::from_secs(2), None)
            .await
            .unwrap();

        assert!(run.outcome.contains("timed out"), "{}", run.outcome);
        let before = ticks();
        assert!(before > 0, "the command never started");

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(ticks(), before, "the backgrounded child survived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        assert!(core().run_shell("   ", &std::env::temp_dir(), Duration::from_secs(1), None).await.is_err());
    }

    #[test]
    fn a_long_tail_is_cut_on_a_line_boundary() {
        assert_eq!(tail_chars("short", 20), "short");
        let cut = tail_chars("aaaa\nbbbb\ncccc", 9);
        assert_eq!(cut, "…\ncccc");
        // nothing to cut on is still cut
        assert!(tail_chars(&"x".repeat(50), 10).starts_with('…'));
    }

    #[tokio::test]
    async fn the_working_directory_is_remembered() {
        let c = core();
        assert!(c.work_dir().await.is_none());
        c.set_work_dir(PathBuf::from("/tmp")).await;
        assert_eq!(c.work_dir().await, Some(PathBuf::from("/tmp")));
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
