<div align="center">

# telepager

**A pager for your coding agent.**

Your agent can message you, keep you posted while it works, and — the useful one —
ask you a question with buttons and *wait* for your answer.

[![npm](https://img.shields.io/npm/v/telepager?color=cb3837&logo=npm)](https://www.npmjs.com/package/telepager)
[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)](#install)

</div>

---

```
Claude Code  ──stdio──▶  telepager  ──https──▶  Telegram  ──▶  your phone
                            ▲                                      │
                            └──────────── your button tap ─────────┘
```

An MCP server that lets Claude Code (or any MCP client) reach you on Telegram.
It speaks MCP over stdio and the Telegram Bot API over HTTPS. No webhook, no
public URL, no port forwarding; it runs on your machine and dials out.

## Tools

| Tool | What it does |
| --- | --- |
| `send_message(text)` | Sends a message. Text over 4096 chars is split across several. |
| `send_thinking(text)` | Shows a `💭 …` status line, **edited in place** on each call instead of spamming the chat. |
| `ask_question(question, options[])` | Sends the question with the options as numbered buttons, blocks until you tap one, and returns the option you picked. |

`ask_question` is the point of the whole thing. The tool call doesn't return, so
your agent is genuinely parked until you answer — it stops and waits instead of
guessing, and you unblock a long task from your phone. When you tap, the message
rewrites itself to `✅ 2. <your answer>` and the buttons disappear.

## Install

```bash
npm install -g telepager
```

The postinstall step downloads the prebuilt binary for your platform (macOS
arm64/x64, Linux arm64/x64, Windows x64) from GitHub Releases and verifies it
against the published sha256 before installing.

Or build it yourself:

```bash
cargo build --release   # -> target/release/telepager
```

## Setup

**1. Make a bot.** Message [@BotFather](https://t.me/BotFather), send `/newbot`,
follow the prompts, and keep the token it gives you. BotFather is only used this
once — after that, telepager talks to `api.telegram.org` directly.

**2. Get your user ID.** Message [@userinfobot](https://t.me/userinfobot); it
replies with your numeric ID.

**3. Send your bot a message** so a chat exists — a bot can't open one with you.

**4. Write the config.** Where it goes depends on the platform:

| Platform | Path |
| --- | --- |
| Linux | `~/.config/telepager/config.json` (or `$XDG_CONFIG_HOME`) |
| macOS | `~/Library/Application Support/telepager/config.json`, or `~/.config/telepager/config.json` |
| Windows | `%APPDATA%\telepager\config.json` |

```json
{
  "bot_token": "123456:ABC-DEF…",
  "allowed_user_ids": [123456789]
}
```

| Key | Default | Meaning |
| --- | --- | --- |
| `bot_token` | — | Required. `TELEGRAM_BOT_TOKEN` overrides it. |
| `allowed_user_ids` | — | Required, non-empty. Button taps from anyone else are ignored. `TELEGRAM_ALLOWED_IDS` (comma-separated) overrides it. |
| `chat_id` | lowest allowed id | Where messages go. For a private chat this is just your user ID, so you rarely set it. |
| `ask_timeout_seconds` | `300` | How long `ask_question` waits before giving up. |

Also looked for at `./telepager.config.json`; `--config PATH` overrides the
lookup entirely.

**5. Register it with your MCP client.**

```bash
claude mcp add --scope user telepager -- telepager
```

<details>
<summary>By hand, or for another client</summary>

```json
{
  "mcpServers": {
    "telepager": {
      "command": "telepager"
    }
  }
}
```

Cursor reads `~/.cursor/mcp.json`, Claude Code reads `~/.claude.json`. Your
client starts and stops the server itself — there's no daemon to babysit.

</details>

**6. Tell your agent to use it.** Nothing forces an agent to page you, so add a
line to your `CLAUDE.md`:

> When you hit a decision you'd otherwise guess at during a long task, use
> telepager's `ask_question` to ask me instead. Page me with `send_message`
> when a long task finishes.

## Security

The allowlist is the entire security model, so telepager **refuses to start with
an empty one**. Beyond that:

- **The token is a secret.** Anyone holding it can act as your bot. Prefer
  `TELEGRAM_BOT_TOKEN` over the plaintext file; the config file is gitignored.
- **Telegram is not end-to-end encrypted.** Its servers can see the traffic —
  don't have your agent page you with real secrets.
- **Errors never carry the token.** The bot token sits in the request URL, so
  every error leaving the client is scrubbed before it reaches a log or your
  agent's context.

telepager cannot run commands, read your files, or touch your machine. It only
sends and receives Telegram messages.

## How it runs

Telegram only allows one poller per bot, so telepager doesn't put one in every
client. The first client to start launches a **daemon** in the background, which
owns the Telegram connection; every client after that is a thin shim that
forwards tool calls to it over a loopback socket, authenticated with a token
kept in a `0600` file. Messages are labelled with the project directory they
came from, so you can tell which agent is talking.

Run as many clients as you like. The daemon starts on demand — there's nothing
to install as a service and nothing to babysit.

## Known limits

- `ask_question` can outlive your MCP client's own tool-call timeout. If your
  client gives up first, lower `ask_timeout_seconds` to match.
- The daemon keeps running once started. `pkill -f "telepager daemon"` stops it;
  the next client starts a fresh one.

## License

[AGPL-3.0](LICENSE). If you modify telepager and distribute it — or run a
modified version as a service — you have to publish your source under the same
license.
