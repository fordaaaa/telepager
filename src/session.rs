//! Sessions and the event stream. A session is one thing we're tracking: an MCP
//! client on the local socket (`Attached`), or an agent process we started
//! (`Spawned`). Same record and event log either way, so the web UI and telegram
//! render one list rather than two.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::screen;

/// Output lines a session keeps — enough to scroll back through a build, few
/// enough that a runaway agent can't eat the machine.
const MAX_EVENTS_PER_SESSION: usize = 2000;

pub type SessionId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An MCP client connected to us.
    Attached,
    /// An agent process we started.
    Spawned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Idle,
    Working,
    /// Blocked on a question the user hasn't answered.
    Waiting,
    Exited { code: Option<i32> },
    Failed { reason: String },
}

impl State {
    pub fn is_over(&self) -> bool {
        matches!(self, State::Exited { .. } | State::Failed { .. })
    }

    /// A short word for the UI badge and the Telegram summary.
    pub fn word(&self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Waiting => "waiting",
            State::Exited { .. } => "exited",
            State::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A line an agent process wrote.
    Output { stream: String, line: String },
    /// A message pushed to Telegram on this session's behalf.
    Message { text: String },
    /// The status line an agent keeps updating.
    Thinking { text: String },
    Question { question: String, options: Vec<String> },
    Answer { answer: String },
    Started { agent: String, task: String },
    Ended { code: Option<i32> },
    StateChanged { state: State },
    /// A turn of the master agent conversation. `role` is user, master or tool.
    Chat { role: String, text: String },
    Notice { text: String },
    /// A pty session's screen moved on. Just the revision — the console fetches
    /// the diff, so no screen dump goes through the bus.
    Screen { revision: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    /// None for events about the app itself rather than one session.
    pub session: Option<SessionId>,
    pub at: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub label: String,
    pub kind: Kind,
    pub cwd: Option<PathBuf>,
    /// Which preset spawned this, for `Spawned` sessions.
    pub agent: Option<String>,
    pub task: Option<String>,
    #[serde(flatten)]
    pub state: State,
    pub created_at: u64,
    /// The status line, if this session has been setting one.
    pub thinking: Option<String>,
    /// The question this session is blocked on, if any.
    pub question: Option<Value>,
    #[serde(skip)]
    pub events: VecDeque<Event>,
    /// The terminal screen, for sessions running on a pty.
    #[serde(skip)]
    pub screen: Option<screen::Shared>,
}

impl Session {
    /// The session list the UI and master both read, minus the event log —
    /// that's fetched per session.
    pub fn summary(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "kind": self.kind,
            "cwd": self.cwd.as_ref().map(|p| p.display().to_string()),
            "agent": self.agent,
            "task": self.task,
            "state": self.state.word(),
            "detail": match &self.state {
                State::Exited { code } => code.map(|c| format!("exit {c}")),
                State::Failed { reason } => Some(reason.clone()),
                _ => None,
            },
            "created_at": self.created_at,
            "thinking": self.thinking,
            "question": self.question,
            // so the console knows to draw a terminal without asking first
            "pty": self.screen.is_some(),
        })
    }

    /// A one-line description for the master agent's tool results.
    pub fn line(&self) -> String {
        let kind = match self.kind {
            Kind::Attached => "attached".to_string(),
            Kind::Spawned => format!("spawned:{}", self.agent.as_deref().unwrap_or("?")),
        };
        let mut out = format!("{} [{}] {} — {}", self.id, kind, self.label, self.state.word());
        if let Some(task) = &self.task {
            out.push_str(&format!(" — task: {}", ellipsis(task, 120)));
        }
        if let Some(q) = &self.question {
            if let Some(text) = q["question"].as_str() {
                out.push_str(&format!(" — waiting on: {}", ellipsis(text, 120)));
            }
        }
        out
    }

    /// The tail of this session's output, for the terminal pane and for the
    /// master when it wants to know what an agent said.
    pub fn output_tail(&self, lines: usize) -> String {
        // a pty session's real output is the screen, not the line log
        if let Some(screen) = &self.screen {
            return screen::lock(screen).tail_text(lines);
        }
        let picked: Vec<&str> = self
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Output { line, .. } => Some(line.as_str()),
                _ => None,
            })
            .collect();
        let start = picked.len().saturating_sub(lines);
        picked[start..].join("\n")
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ellipsis(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    s.chars().take(limit).collect::<String>() + "…"
}

/// The session registry and the event bus in front of it. Every mutation
/// records an event, so a watcher sees the same history a late joiner rebuilds
/// from `snapshot`.
pub struct Registry {
    sessions: BTreeMap<SessionId, Session>,
    /// Ordering key, and what an SSE client sends back as Last-Event-ID.
    seq: AtomicU64,
    next_id: u64,
    tx: broadcast::Sender<Event>,
    /// A short scrollback of app-level events for a page that just opened.
    recent: VecDeque<Event>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            sessions: BTreeMap::new(),
            seq: AtomicU64::new(0),
            next_id: 1,
            tx,
            recent: VecDeque::new(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Sessions in creation order, newest last.
    pub fn list(&self) -> Vec<&Session> {
        let mut all: Vec<&Session> = self.sessions.values().collect();
        all.sort_by_key(|s| s.created_at);
        all
    }

    pub fn summaries(&self) -> Vec<Value> {
        self.list().iter().map(|s| s.summary()).collect()
    }

    /// Sessions that could still do something, for the master agent's context.
    pub fn live(&self) -> Vec<&Session> {
        self.list().into_iter().filter(|s| !s.state.is_over()).collect()
    }

    pub fn open(
        &mut self,
        kind: Kind,
        label: String,
        cwd: Option<PathBuf>,
        agent: Option<String>,
        task: Option<String>,
    ) -> SessionId {
        let id = format!("s{}", self.next_id);
        self.next_id += 1;

        self.sessions.insert(
            id.clone(),
            Session {
                id: id.clone(),
                label,
                kind,
                cwd,
                agent: agent.clone(),
                task: task.clone(),
                state: State::Idle,
                created_at: now(),
                thinking: None,
                question: None,
                events: VecDeque::new(),
                screen: None,
            },
        );

        if let (Some(agent), Some(task)) = (agent, task) {
            self.record(Some(&id), EventKind::Started { agent, task });
        } else {
            self.record(Some(&id), EventKind::Notice { text: "session connected".into() });
        }
        id
    }

    /// Give a session a screen, once we know it's running on a pty.
    pub fn attach_screen(&mut self, id: &str, screen: screen::Shared) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.screen = Some(screen);
        }
    }

    pub fn screen(&self, id: &str) -> Option<screen::Shared> {
        self.sessions.get(id).and_then(|s| s.screen.clone())
    }

    pub fn set_state(&mut self, id: &str, state: State) {
        let changed = match self.sessions.get_mut(id) {
            Some(s) if s.state != state => {
                s.state = state.clone();
                if !matches!(state, State::Waiting) {
                    s.question = None;
                }
                true
            }
            _ => false,
        };
        if changed {
            self.record(Some(id), EventKind::StateChanged { state });
        }
    }

    pub fn set_thinking(&mut self, id: &str, text: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.thinking = Some(text.to_string());
        }
        self.record(Some(id), EventKind::Thinking { text: text.to_string() });
    }

    pub fn set_question(&mut self, id: &str, question: &str, options: &[String]) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.question = Some(json!({ "question": question, "options": options }));
            s.state = State::Waiting;
        }
        self.record(
            Some(id),
            EventKind::Question { question: question.to_string(), options: options.to_vec() },
        );
        self.record(Some(id), EventKind::StateChanged { state: State::Waiting });
    }

    pub fn clear_question(&mut self, id: &str, answer: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.question = None;
            if s.state == State::Waiting {
                s.state = State::Working;
            }
        }
        self.record(Some(id), EventKind::Answer { answer: answer.to_string() });
    }

    pub fn close(&mut self, id: &str, code: Option<i32>) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.state = State::Exited { code };
            s.question = None;
            s.thinking = None;
        }
        self.record(Some(id), EventKind::Ended { code });
    }

    pub fn fail(&mut self, id: &str, reason: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.state = State::Failed { reason: reason.to_string() };
        }
        self.record(Some(id), EventKind::StateChanged {
            state: State::Failed { reason: reason.to_string() },
        });
    }

    /// Drop finished sessions, keeping the most recent few for scrollback.
    pub fn prune(&mut self, keep_finished: usize) {
        let mut finished: Vec<(u64, SessionId)> = self
            .sessions
            .values()
            .filter(|s| s.state.is_over())
            .map(|s| (s.created_at, s.id.clone()))
            .collect();
        if finished.len() <= keep_finished {
            return;
        }
        finished.sort();
        let cut = finished.len() - keep_finished;
        for (_, id) in finished.into_iter().take(cut) {
            self.sessions.remove(&id);
        }
    }

    pub fn forget(&mut self, id: &str) -> bool {
        self.sessions.remove(id).is_some()
    }

    /// Append an event to a session's log and publish it on the bus.
    pub fn record(&mut self, session: Option<&str>, kind: EventKind) -> Event {
        let event = Event {
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            session: session.map(str::to_string),
            at: now(),
            kind,
        };

        match session.and_then(|id| self.sessions.get_mut(id)) {
            Some(s) => {
                s.events.push_back(event.clone());
                while s.events.len() > MAX_EVENTS_PER_SESSION {
                    s.events.pop_front();
                }
            }
            None => {
                self.recent.push_back(event.clone());
                while self.recent.len() > 200 {
                    self.recent.pop_front();
                }
            }
        }

        // no subscribers is the normal case when nothing has the ui open
        let _ = self.tx.send(event.clone());
        event
    }

    /// Everything a page needs to draw itself on first load.
    pub fn snapshot(&self, session: Option<&str>, limit: usize) -> Vec<Event> {
        // only the tail is ever returned, and this runs under the lock every
        // output line takes — so clone what's returned, not the whole log
        match session {
            Some(id) => self
                .sessions
                .get(id)
                .map(|s| s.events.iter().rev().take(limit).rev().cloned().collect())
                .unwrap_or_default(),
            None => {
                // each log is already ordered by seq, so nothing outside a
                // log's own last `limit` can make the merged cut
                let mut all: Vec<&Event> = self.recent.iter().rev().take(limit).rev().collect();
                for s in self.sessions.values() {
                    all.extend(s.events.iter().rev().take(limit).rev());
                }
                all.sort_by_key(|e| e.seq);
                let start = all.len().saturating_sub(limit);
                all[start..].iter().map(|e| (*e).clone()).collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::new()
    }

    #[test]
    fn opening_a_session_gives_it_an_id_and_an_event() {
        let mut r = registry();
        let id = r.open(Kind::Attached, "telepager".into(), None, None, None);
        assert_eq!(id, "s1");
        assert_eq!(r.get(&id).unwrap().events.len(), 1);

        let second = r.open(Kind::Attached, "other".into(), None, None, None);
        assert_eq!(second, "s2");
    }

    #[test]
    fn a_spawned_session_records_the_task() {
        let mut r = registry();
        let id = r.open(
            Kind::Spawned,
            "repo".into(),
            Some(PathBuf::from("/tmp")),
            Some("claude".into()),
            Some("fix the build".into()),
        );
        let s = r.get(&id).unwrap();
        assert_eq!(s.agent.as_deref(), Some("claude"));
        assert!(matches!(s.events[0].kind, EventKind::Started { .. }));
        assert!(s.line().contains("fix the build"));
    }

    #[test]
    fn asking_a_question_parks_the_session_and_answering_frees_it() {
        let mut r = registry();
        let id = r.open(Kind::Attached, "x".into(), None, None, None);

        r.set_question(&id, "ship it?", &["yes".into(), "no".into()]);
        assert_eq!(r.get(&id).unwrap().state, State::Waiting);
        assert!(r.get(&id).unwrap().question.is_some());

        r.clear_question(&id, "yes");
        assert_eq!(r.get(&id).unwrap().state, State::Working);
        assert!(r.get(&id).unwrap().question.is_none());
    }

    #[test]
    fn state_changes_only_record_when_something_changed() {
        let mut r = registry();
        let id = r.open(Kind::Attached, "x".into(), None, None, None);
        let before = r.get(&id).unwrap().events.len();

        r.set_state(&id, State::Working);
        r.set_state(&id, State::Working);

        assert_eq!(r.get(&id).unwrap().events.len(), before + 1);
    }

    #[test]
    fn closing_a_session_marks_it_over_and_clears_the_question() {
        let mut r = registry();
        let id = r.open(Kind::Spawned, "x".into(), None, Some("claude".into()), Some("t".into()));
        r.set_question(&id, "q", &["a".into()]);

        r.close(&id, Some(0));

        let s = r.get(&id).unwrap();
        assert!(s.state.is_over());
        assert!(s.question.is_none());
        assert!(r.live().is_empty());
    }

    #[test]
    fn the_event_log_is_capped() {
        let mut r = registry();
        let id = r.open(Kind::Spawned, "x".into(), None, None, None);
        for i in 0..(MAX_EVENTS_PER_SESSION + 50) {
            r.record(Some(&id), EventKind::Output { stream: "stdout".into(), line: format!("line {i}") });
        }
        assert_eq!(r.get(&id).unwrap().events.len(), MAX_EVENTS_PER_SESSION);
        // the oldest lines are the ones that went
        assert!(r.get(&id).unwrap().output_tail(1).contains(&format!("line {}", MAX_EVENTS_PER_SESSION + 49)));
    }

    #[test]
    fn output_tail_returns_only_output_lines_newest_last() {
        let mut r = registry();
        let id = r.open(Kind::Spawned, "x".into(), None, None, None);
        r.record(Some(&id), EventKind::Output { stream: "stdout".into(), line: "one".into() });
        r.record(Some(&id), EventKind::Notice { text: "ignore me".into() });
        r.record(Some(&id), EventKind::Output { stream: "stderr".into(), line: "two".into() });

        assert_eq!(r.get(&id).unwrap().output_tail(10), "one\ntwo");
        assert_eq!(r.get(&id).unwrap().output_tail(1), "two");
    }

    #[test]
    fn a_pty_sessions_tail_comes_from_its_screen() {
        use std::sync::{Arc, Mutex};

        let mut r = registry();
        let id = r.open(Kind::Spawned, "x".into(), None, None, None);
        r.record(Some(&id), EventKind::Output { stream: "screen".into(), line: "old".into() });

        let screen = Arc::new(Mutex::new(crate::screen::Screen::new(20, 4)));
        crate::screen::lock(&screen).feed(b"drawn\r\nin place");
        r.attach_screen(&id, screen);

        assert_eq!(r.get(&id).unwrap().output_tail(10), "drawn\nin place");
        assert!(r.screen(&id).is_some());
    }

    #[test]
    fn sequence_numbers_increase_across_sessions() {
        let mut r = registry();
        let a = r.open(Kind::Attached, "a".into(), None, None, None);
        let b = r.open(Kind::Attached, "b".into(), None, None, None);
        let first = r.record(Some(&a), EventKind::Notice { text: "1".into() });
        let second = r.record(Some(&b), EventKind::Notice { text: "2".into() });
        assert!(second.seq > first.seq);
    }

    #[test]
    fn subscribers_see_events_as_they_happen() {
        let mut r = registry();
        let mut rx = r.subscribe();
        let id = r.open(Kind::Attached, "x".into(), None, None, None);
        let event = rx.try_recv().expect("the open should have been published");
        assert_eq!(event.session.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn snapshot_merges_sessions_in_sequence_order() {
        let mut r = registry();
        let a = r.open(Kind::Attached, "a".into(), None, None, None);
        let b = r.open(Kind::Attached, "b".into(), None, None, None);
        r.record(Some(&a), EventKind::Notice { text: "from a".into() });
        r.record(Some(&b), EventKind::Notice { text: "from b".into() });

        let all = r.snapshot(None, 100);
        let seqs: Vec<u64> = all.iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort();
        assert_eq!(seqs, sorted);

        let just_a = r.snapshot(Some(&a), 100);
        assert!(just_a.iter().all(|e| e.session.as_deref() == Some(a.as_str())));
    }

    #[test]
    fn snapshot_returns_the_newest_events_up_to_the_limit() {
        let mut r = registry();
        let a = r.open(Kind::Attached, "a".into(), None, None, None);
        let b = r.open(Kind::Attached, "b".into(), None, None, None);
        for i in 0..20 {
            r.record(Some(&a), EventKind::Notice { text: format!("a{i}") });
            r.record(Some(&b), EventKind::Notice { text: format!("b{i}") });
        }

        let merged = r.snapshot(None, 5);
        assert_eq!(merged.len(), 5);
        // the newest five overall, still in order
        let seqs: Vec<u64> = merged.iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort();
        assert_eq!(seqs, sorted);
        assert_eq!(merged.last().map(|e| e.seq), Some(r.seq.load(Ordering::Relaxed)));

        let just_a = r.snapshot(Some(&a), 5);
        assert_eq!(just_a.len(), 5);
        assert!(just_a.iter().all(|e| e.session.as_deref() == Some(a.as_str())));
        assert!(matches!(&just_a[0].kind, EventKind::Notice { text } if text == "a15"));
    }

    #[test]
    fn pruning_keeps_the_newest_finished_sessions_and_all_live_ones() {
        let mut r = registry();
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = r.open(Kind::Spawned, format!("s{i}"), None, None, None);
            r.close(&id, Some(0));
            ids.push(id);
        }
        let live = r.open(Kind::Attached, "live".into(), None, None, None);

        r.prune(2);

        assert!(r.get(&live).is_some());
        assert!(r.get(&ids[0]).is_none());
        assert!(r.get(&ids[4]).is_some());
        assert_eq!(r.list().len(), 3);
    }

    #[test]
    fn a_session_summary_carries_the_state_word() {
        let mut r = registry();
        let id = r.open(Kind::Spawned, "x".into(), None, Some("claude".into()), Some("t".into()));
        r.fail(&id, "command not found");

        let summary = r.get(&id).unwrap().summary();
        assert_eq!(summary["state"], "failed");
        assert_eq!(summary["detail"], "command not found");
        assert_eq!(summary["agent"], "claude");
    }
}
