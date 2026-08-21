# telepager as an app

Today telepager is an MCP server that happens to run a background daemon. The
plan is to flip that: telepager becomes a standalone program you install and
run, and the MCP server is one component it ships, which agents connect to.

## The shape

```
                    ┌─────────────────────────────────────┐
                    │            telepager app            │
  browser ──http──▶ │  web ui  ─┐                         │
                    │           ├─ core: sessions, events │ ──https──▶ telegram
  claude code ──────│  mcp shim ┘        agent supervisor │
      (stdio)       │                          │          │
                    └──────────────────────────┼──────────┘
                                               ▼
                                      spawned agent processes
```

One process owns everything: the Telegram connection, the session registry, the
event stream, and any agents it spawned. Everything else is a client of it.

## Components

**core (daemon).** Already exists. Owns the single Telegram connection, the
session registry and question routing. Grows an event bus and process
supervision.

**mcp shim.** Already exists as the default binary mode. Becomes an explicit
subcommand so the app itself can own the bare `telepager` command.

**web ui.** Served by core on loopback. Tabs are sessions, the terminal pane is
the output of spawned agents, questions can be answered from the browser.

**agent supervisor.** Launches agent CLIs in allowlisted directories, owns their
stdout/stderr, streams it to the ui and to telegram.

**setup wizard.** First run: paste a bot token, discover the user id by waiting
for a message, register with whichever MCP clients are installed.

## Commands

| Command | What it does |
| --- | --- |
| `telepager` | starts the app, opens the ui, runs setup if unconfigured |
| `telepager mcp` | the stdio server an MCP client spawns |
| `telepager daemon` | core on its own, no ui |
| `telepager setup` | rerun the wizard |
| `telepager status` | is it running, which sessions, what's pending |

`telepager mcp` is a **breaking change** for anyone already registered with the
bare command. The shim should keep working with no arguments for a release or
two, printing a deprecation notice to stderr.

## Sessions

Right now a session is a socket connection labelled with the client's working
directory. That needs to become a real record:

- stable id, label, working directory
- kind: `attached` (an MCP client connected) or `spawned` (we launched it)
- state: idle, working, waiting on an answer, exited
- an event log: messages sent, questions asked, answers given, output lines

Both the ui and telegram topics render the same session list. One model, two
front ends.

## Status

Phases 0–7 are done. telepager is the app described above: one background
process owning Telegram, the session registry, the spawned agents and the
console, with the MCP shim as one client of it. The master agent landed with
phase 4 and runs on any of four model providers.

What's left from the original sketch: Telegram topics (phase 6) is unbuilt —
the phone gets one chat and `/status` rather than a thread per session.

## Phases

**0. Ship 0.1.0.** ✅ Fix the `macos-13` runner in the release matrix by
cross-compiling the intel mac binary from the arm runner. Finish the npm
publish. Do not start the refactor on top of an unreleased tree.

**1. Command restructure.** ✅ Subcommands as above, `telepager mcp` added with the
bare form still working. Registration docs updated. No behaviour change.

**2. Session model + event bus.** ✅ Sessions become records with ids and state.
Core gains a broadcast channel of events. Nothing consumes it yet except
`telepager status`.

**3. Web ui, read only.** ✅ Loopback http server, session tabs, live event log via
server-sent events, daemon health. Proves the plumbing with no new risk.

**4. Agent spawning.** ✅ `@spawnagent <dir> <task>` from telegram, and a spawn
button in the ui. Directory allowlist, confirm button before each spawn. Output
streams to the ui terminal pane and, condensed, to telegram.

**5. Interactive ui.** ✅ Answer questions from the browser, kill a spawned agent,
re-run a task.

**6. Telegram topics.** — not built. A supergroup with topics, one thread per session, so the
phone gets the same tabs as the browser.

**7. Packaging.** ✅ Embed the ui assets in the binary. Replace the npm postinstall
download with per-platform optional dependencies so `ignore-scripts` installs
still work. Ship the wizard.

## The master agent

Not a router. Routing an answer back to the session that asked is a hashmap
lookup and should stay one — putting a model in that path adds latency and a new
way to be wrong.

The master agent is the thing you *talk to*. You message it in telegram or the ui
without addressing any particular session, and it decides what to do: spawn a
worker in some directory, summarise what the running sessions are doing, kill one
that's stuck, answer a question on your behalf if you've told it how.

That makes it a client of core like everything else — it holds the conversation,
calls the same spawn and status apis the ui does, and has no privileges the ui
doesn't. Which also means it can be added last, and telepager works without it.

Lands with phase 4, since it has nothing to orchestrate until spawning exists.

## Decisions made

- **Which agent CLIs.** Ten are built in — claude, codex, gemini, opencode,
  cursor-agent, amp, aider, crush, qwen, goose — and only the ones actually on
  PATH are offered. Anything else is a config entry, no code change.
- **Does the app run headless by default?** No: `telepager` opens a browser,
  because that's what makes it one command. `--no-open` skips it and prints the
  url instead, and `telepager daemon` never opens one. Both are what you want
  over ssh.
- **Binary size.** 6.2mb with the console embedded, up from 5.5mb. Fine.
- **Which model runs the master agent.** Any of anthropic, openai-shaped,
  gemini or ollama, picked in the console. Defaulting to one vendor would have
  made the whole thing a Claude accessory.

## Risks

**The security promise changed.** The readme used to say telepager cannot run
commands or touch your machine. That sentence is gone: a bot token is now a way
to run code here. What keeps it honest is the directory allowlist, which is
empty by default — so Telegram cannot spawn anything until you opt in — and the
split between the remote surface (Telegram, gated) and the local one (the
console, not gated, because reaching it means being at the machine).

**Scope.** Phases 0–3 are additive and safe. Phase 4 onward is a different
product with a different maintenance burden. It's worth deciding at that point
whether telepager is a pager or a console.

**Windows.** Process supervision and killing process trees differ enough that
spawning will need a separate implementation and a real machine to test on.
