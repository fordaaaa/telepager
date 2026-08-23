<div align="center">

# telepager

**Run and watch coding agents from your phone.**

[![npm](https://img.shields.io/npm/v/telepager?color=cb3837&logo=npm)](https://www.npmjs.com/package/telepager)
[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)](#install)

</div>

---

Start an agent in a directory, watch it work, answer questions it gets stuck
on, and kill it when it goes wrong — from Telegram or a local console. Runs
entirely on your machine; no webhook or public URL needed.

## Install

telepager is a terminal command, like `claude` or `git`. You install it, type
`telepager`, and it runs in that terminal. It doesn't install a background
service, doesn't add a login item, and doesn't start with your machine —
running the command is what starts it.

```bash
npm install -g telepager     # or: npx -y telepager, no install
```

```bash
cargo install telepager      # if you have rust
```

```bash
curl -fsSL https://raw.githubusercontent.com/fordaaaa/telepager/main/install.sh | sh
```

The script drops a prebuilt binary in `~/.local/bin`. On Windows use npm or
cargo. From a checkout, `cargo build --release` puts one in `target/release`.

## Run it

```bash
telepager
```

That runs in the foreground and opens the console in your browser. Ctrl-C ends
it; closing the terminal ends it. First run walks you through connecting
Telegram: paste a bot token from [@BotFather](https://t.me/BotFather), press
**Detect**, message your bot, done.

To let your coding agent page you, register the MCP server once:

```bash
claude mcp add --scope user telepager -- telepager mcp
```

| Command | What it does |
| --- | --- |
| `telepager` | run it here, open the console. Ctrl-C ends it |
| `telepager start` | run it in the background instead |
| `telepager stop` | stop the background one |
| `telepager status` | check setup, running state, active model |
| `telepager webui` | open the console against whatever's already running |
| `telepager mcp` | stdio MCP server (spawned by MCP clients) |

<details>
<summary>If <code>npm install -g</code> fails with <code>EACCES</code></summary>

npm is trying to write somewhere you don't own. Point it at your home directory
rather than reaching for `sudo`:

```bash
npm config set prefix ~/.local
```

</details>

## Using it

Talk to the **master agent** in Telegram or the console's chat pane — it's
what you address when you're not talking to a specific session:

> **you:** start claude in ~/code/api and make the tests pass
> **telepager:** Started claude in /home/you/code/api — session s3.
>
> **you:** how's it going?
> **telepager:** It's rewritten the fixtures and is on the last two failures.

It starts workers, summarizes what they're doing, reads their output, sends
follow-ups, kills stuck ones, and answers questions a worker is blocked on.

Worker agents are ordinary coding CLIs — anything installed on your machine
shows up in the picker with no config: `claude` · `codex` · `gemini` ·
`opencode` · `cursor-agent` · `amp` · `aider` · `crush` · `qwen` · `goose`.

### Master agent provider

No key needed — reuse a CLI you're already logged into:

```json
{ "master": { "provider": "claude-code" } }
```
```json
{ "master": { "provider": "opencode", "model": "opencode/nemotron-3.5-lightning-free" } }
```

Or a model API directly, set via env var or in the console's **Settings**:

| Provider | Set | Notes |
| --- | --- | --- |
| `anthropic` | `ANTHROPIC_API_KEY` | |
| `openai` | `OPENAI_API_KEY` | also OpenRouter, Groq, Together, LM Studio, vLLM via `base_url` |
| `gemini` | `GEMINI_API_KEY` | |
| `ollama` | — | runs locally, no key |

```json
{
  "master": {
    "provider": "openai",
    "model": "anthropic/claude-sonnet-4.5",
    "base_url": "https://openrouter.ai/api/v1"
  }
}
```

### Custom worker agents

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

`{task}` is passed as a single argument — no shell, so quotes and semicolons
in a task are inert.

Set `"pty": true` to run an agent on a real terminal, so the console draws its
screen instead of a line log. The `-tui` presets do this. The trade: telepager
holds that terminal, so a `pty` agent ends when telepager does, while a piped
one keeps going without it.

### Shell access

Off until you turn it on, in the console's settings or in the config:

```json
{
  "permissions": {
    "shell": false,
    "remote_control": false,
    "confirm_destructive": true
  }
}
```

`shell` lets the master agent run commands — from Telegram only inside
`allowed_dirs`. `remote_control` lets these settings be changed from Telegram.
`confirm_destructive` asks first when a command looks like it deletes
something. Every run is announced to everyone on the allowlist.

## As an MCP server

Let an agent page *you*:

```bash
claude mcp add --scope user telepager -- telepager mcp
```

| Tool | What it does |
| --- | --- |
| `send_message(text)` | send a message (splits at 4096 chars) |
| `send_thinking(text)` | a `💭 …` status line, edited in place |
| `ask_question(question, options[])` | blocks until you answer; returns your pick |

Add to your `CLAUDE.md` so agents actually use it:

> When you hit a decision you'd otherwise guess at during a long task, use
> telepager's `ask_question` to ask me instead. Page me with `send_message`
> when a long task finishes.

## Configuration

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
| `master` | claude-code | `claude-code`, `opencode`, `anthropic`, `openai`, `gemini`, or `ollama`. |
| `agents` | built-in list | Worker agent CLIs. Yours override built-ins of the same name. |
| `allowed_dirs` | `[]` | Directories Telegram may start agents in. Empty disables it. |
| `ui_port` | any free port | Pin the console's port. |

## Security

**telepager runs code on your machine.**

- `allowed_user_ids` must be non-empty — telepager refuses to start otherwise.
  Messages from anyone else are ignored.
- `allowed_dirs` is empty by default, so Telegram can't spawn agents anywhere
  until you set it. The local console is unrestricted — using it means you're
  already at the machine.
- The console is loopback-only, on a random port, behind a one-off key in the
  URL, and checks the `Host` header.
- Prefer `TELEGRAM_BOT_TOKEN` over the plaintext config; the file is `0600`.
  Anyone with the bot token can act as your bot.
- Telegram isn't end-to-end encrypted — don't page real secrets through it.
- Turning `shell` on means whoever has your Telegram can run commands as you.
  `allowed_dirs` is the boundary; the destructive-command prompt is a seatbelt,
  not a fence — a shell inside an allowed directory can still reach the rest of
  your machine.
- Keys are scrubbed from every error before it's logged or sent anywhere.

To keep the old, inert pager-only behavior: leave `allowed_dirs` empty and
skip configuring a master agent.

## Known limits

- `ask_question` can outlive your MCP client's own tool-call timeout — lower
  `ask_timeout_seconds` to match if it does.
- Spawned agents are killed by process group on Unix; on Windows a killed
  agent may leave grandchildren behind.
- `telepager start` leaves a process running after your terminal is gone;
  `telepager stop` ends it.
- An agent on a `pty` ends when telepager does. Use the headless presets for
  anything that should outlive it.

## License

[AGPL-3.0](LICENSE). If you modify telepager and distribute it — or run a
modified version as a service — you have to publish your source under the
same license.
