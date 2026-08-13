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

## Phases

**0. Ship 0.1.0.** Fix the `macos-13` runner in the release matrix by
cross-compiling the intel mac binary from the arm runner. Finish the npm
publish. Do not start the refactor on top of an unreleased tree.

**1. Command restructure.** Subcommands as above, `telepager mcp` added with the
bare form still working. Registration docs updated. No behaviour change.

**2. Session model + event bus.** Sessions become records with ids and state.
Core gains a broadcast channel of events. Nothing consumes it yet except
`telepager status`.

**3. Web ui, read only.** Loopback http server, session tabs, live event log via
server-sent events, daemon health. Proves the plumbing with no new risk.

**4. Agent spawning.** `@spawnagent <dir> <task>` from telegram, and a spawn
button in the ui. Directory allowlist, confirm button before each spawn. Output
streams to the ui terminal pane and, condensed, to telegram.

**5. Interactive ui.** Answer questions from the browser, kill a spawned agent,
re-run a task.

**6. Telegram topics.** A supergroup with topics, one thread per session, so the
phone gets the same tabs as the browser.

**7. Packaging.** Embed the ui assets in the binary. Replace the npm postinstall
download with per-platform optional dependencies so `ignore-scripts` installs
still work. Ship the wizard.

## Decisions needed

- **Which agent CLIs.** `claude -p` first. The spawn command should be
  configurable so opencode and others work without code changes.
- **Does the app run headless by default?** If `telepager` always opens a
  browser, servers and ssh sessions get awkward. Probably `telepager --no-ui`.
- **Binary size.** Embedded assets will push it past 6mb. Fine, but it's a real
  jump from the current 5.5mb.

## Risks

**The security promise changes.** The readme currently says telepager cannot run
commands or touch your machine. Phase 4 deletes that sentence: a bot token would
become a way to run code here. The directory allowlist and the confirm button
are what keep it honest, and an empty allowlist keeps the feature inert for
anyone who hasn't opted in.

**Scope.** Phases 0–3 are additive and safe. Phase 4 onward is a different
product with a different maintenance burden. It's worth deciding at that point
whether telepager is a pager or a console.

**Windows.** Process supervision and killing process trees differ enough that
spawning will need a separate implementation and a real machine to test on.
