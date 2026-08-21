<div align="center">

# telepager

**Run and watch coding agents from your phone.**

Start an agent in a directory, watch it work, answer the questions it gets stuck
on, and kill it when it goes wrong — from Telegram, or from a console on your
own machine.

[![npm](https://img.shields.io/npm/v/telepager?color=cb3837&logo=npm)](https://www.npmjs.com/package/telepager)
[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)](#install)

</div>

---

```
                    ┌─────────────────────────────────────┐
  browser ──http──▶ │  console ─┐                         │
                    │           ├─ sessions, events,      │ ──https──▶ Telegram
  Claude Code ──────│  mcp shim ┘   master agent,         │               │
      (stdio)       │               agent supervisor      │               ▼
                    └───────────────────┬─────────────────┘          your phone
                                        ▼
                          claude · codex · gemini · opencode …
```

One command starts everything. No webhook, no public URL, no port forwarding —
it runs on your machine and dials out.

## Start

```bash
npm install -g telepager
telepager
```

That's it. `telepager` starts the app and opens the console in your browser. If
Telegram isn't connected yet, the page walks you through it — paste a bot token
from [@BotFather](https://t.me/BotFather), press **Detect**, message your bot,
done. The app keeps running in the background; `telepager stop` ends it.

| Command | What it does |
| --- | --- |
| `telepager` | start the app and open the console |
| `telepager webui` | the same thing, said out loud |
| `telepager status` | is it set up, is it running, which model |
| `telepager stop` | stop the background app |
| `telepager mcp` | the stdio server an MCP client spawns |
| `telepager daemon` | run in the foreground, no browser |

<details>
<summary>Other ways to install</summary>

**Without installing anything** — `npx` fetches it on demand:

```bash
npx -y telepager
```

**The install script**, if you'd rather have a plain binary:

```bash
curl -fsSL https://raw.githubusercontent.com/fordaaaa/telepager/main/install.sh | sh
```

**From source:**

```bash
cargo build --release   # -> target/release/telepager
```

If `npm install -g` fails with `EACCES`, npm is trying to write somewhere you
don't own. Point it at your home directory — this fixes every global npm
package, not just this one:

```bash
npm config set prefix ~/.local
```

Don't reach for `sudo`: you'd be running a downloaded executable as root.

</details>

## The master agent

You talk to it in Telegram, or in the console's chat pane. It isn't a router —
answering a question still goes straight back to the session that asked. It's
the thing you *talk to* when you're not addressing any particular session:

> **you:** start claude in ~/code/api and make the tests pass
> **telepager:** Started claude in /home/you/code/api — session s3.
>
> **you:** how's it going?
> **telepager:** It's rewritten the fixtures and is on the last two failures.

It can start workers, summarise what they're doing, read their output, type
follow-ups at them, kill stuck ones, and — when you've told it how — answer a
question a worker is blocked on.

### It isn't only Claude

The master agent runs on whichever model you point it at:

| Provider | Set | Notes |
| --- | --- | --- |
| `anthropic` | `ANTHROPIC_API_KEY` | the default |
| `openai` | `OPENAI_API_KEY` | also OpenRouter, Groq, Together, LM Studio, vLLM — anything OpenAI-shaped, via `base_url` |
| `gemini` | `GEMINI_API_KEY` | |
| `ollama` | — | runs locally, no key |

Pick one in the console under **Settings**, or write it yourself:

```json
{
  "master": {
    "provider": "openai",
    "model": "anthropic/claude-sonnet-4.5",
    "base_url": "https://openrouter.ai/api/v1"
  }
}
```

If you already have a key exported, telepager finds it — `OPENROUTER_API_KEY`,
`GROQ_API_KEY` and friends are all checked before it gives up.

## Worker agents

The agents the master starts are ordinary coding CLIs. Anything installed on
your machine shows up in the picker with no configuration:

`claude` · `codex` · `gemini` · `opencode` · `cursor-agent` · `amp` · `aider`
· `crush` · `qwen` · `goose`

Add your own, or change how one is invoked, in the config file. `{task}` is
replaced with the task text as a single argument — no shell, so quotes and
semicolons in a task are inert:

```json
{
  "agents": {
    "mine": {
      "command": "my-agent",
      "args": ["--prompt", "{task}", "--cwd", "{dir}"],
      "description": "shows up in the picker"
    }
  }
}
```

## As an MCP server

telepager still does what it always did: let an agent page *you*.

```bash
claude mcp add --scope user telepager -- telepager mcp
```

| Tool | What it does |
| --- | --- |
| `send_message(text)` | Sends a message. Text over 4096 chars is split across several. |
| `send_thinking(text)` | Shows a `💭 …` status line, **edited in place** instead of spamming the chat. |
| `ask_question(question, options[])` | Sends the question with numbered buttons, blocks until you answer, and returns what you picked. |

`ask_question` is the point of the whole thing. The tool call doesn't return, so
your agent is genuinely parked until you answer — it stops and waits instead of
guessing. Answer from Telegram or from the console; both reach the same session.

Nothing forces an agent to page you, so add a line to your `CLAUDE.md`:

> When you hit a decision you'd otherwise guess at during a long task, use
> telepager's `ask_question` to ask me instead. Page me with `send_message`
> when a long task finishes.

## Security

**telepager runs code on your machine.** That's the feature, and it's a real
change from what it used to be — older versions couldn't touch anything but
Telegram messages. What keeps it honest:

- **The allowlist is the whole model.** telepager refuses to start with an empty
  `allowed_user_ids`. Messages and taps from anyone else are ignored.
- **Telegram can only start agents in directories you've allowed.** `allowed_dirs`
  is empty by default, which means Telegram can't spawn anything at all. Set it
  in the console under Settings. The local console isn't restricted — opening it
  means you're already at the machine.
- **The console is loopback-only**, on a random port, behind a one-off key in the
  URL, and refuses requests whose `Host` isn't localhost.
- **The bot token is a secret.** Anyone holding it can act as your bot. Prefer
  `TELEGRAM_BOT_TOKEN` over the plaintext file; the config is written `0600`.
- **Telegram is not end-to-end encrypted.** Don't have your agent page you with
  real secrets.
- **Keys never reach a log.** Both the bot token and your model key are scrubbed
  out of every error before it goes anywhere.

If you want the old, inert behaviour: leave `allowed_dirs` empty and don't
configure a master agent. Spawning is then unreachable from Telegram, and
telepager is a pager again.

## Configuration

Where the file lives:

| Platform | Path |
| --- | --- |
| Linux | `~/.config/telepager/config.json` (or `$XDG_CONFIG_HOME`) |
| macOS | `~/Library/Application Support/telepager/config.json` |
| Windows | `%APPDATA%\telepager\config.json` |

Also read from `./telepager.config.json`; `--config PATH` overrides the lookup.

| Key | Default | Meaning |
| --- | --- | --- |
| `bot_token` | — | Required. `TELEGRAM_BOT_TOKEN` overrides it. |
| `allowed_user_ids` | — | Required, non-empty. `TELEGRAM_ALLOWED_IDS` (comma-separated) overrides it. |
| `chat_id` | lowest allowed id | Where messages go. Rarely set. |
| `ask_timeout_seconds` | `300` | How long `ask_question` waits before giving up. |
| `master` | anthropic | The model the master agent runs on. See above. |
| `agents` | the built-in list | Worker agent CLIs. Yours override built-ins of the same name. |
| `allowed_dirs` | `[]` | Directories Telegram may start agents in. Empty disables it. |
| `ui_port` | any free port | Pin the console's port. |

## How it runs

Telegram allows one poller per bot, so telepager runs one background process
that owns everything: the Telegram connection, the session registry, the agents
it spawned, and the console. Everything else — the MCP shims, your browser — is
a client of it over loopback, authenticated with a token in a `0600` file.

The first thing that needs it starts it, whether that's `telepager`, `telepager
webui`, or an MCP client launching the shim. Run as many clients as you like.

## Known limits

- `ask_question` can outlive your MCP client's own tool-call timeout. If your
  client gives up first, lower `ask_timeout_seconds` to match.
- Spawned agents are killed by process group on Unix. On Windows a killed agent
  may leave grandchildren behind.
- The app keeps running once started. `telepager stop` ends it.

## License

[AGPL-3.0](LICENSE). If you modify telepager and distribute it — or run a
modified version as a service — you have to publish your source under the same
license.
