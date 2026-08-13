# telepager

**A pager for your coding agent.** An MCP server that lets Claude Code (or any
MCP client) reach you on Telegram: send you a message, keep a status line
updated while it works, and — the useful one — ask you a question with buttons
and *wait* for your answer.

It speaks MCP over stdio and the Telegram Bot API over HTTPS. No webhook, no
public URL, no port forwarding; it runs on your machine and dials out.

```
Claude Code  ──stdio──▶  telepager  ──https──▶  Telegram  ──▶  your phone
                            ▲                                      │
                            └──────────── your button tap ─────────┘
```

## Tools

| Tool | What it does |
| --- | --- |
| `send_message(text)` | Sends a message. Text over 4096 chars is split across several. |
| `send_thinking(text)` | Shows a `💭 …` status line, **edited in place** on each call instead of spamming the chat. |
| `ask_question(question, options[])` | Sends the question with the options as numbered buttons, blocks until you tap one, and returns the option you picked. |

`ask_question` is the point of the whole thing: the agent stops and waits for a
real answer instead of guessing, so you can unblock a long task from your phone.
When you tap, the message rewrites itself to `✅ 2. <your answer>` and the
buttons disappear.

## Setup

**1. Make a bot.** Message [@BotFather](https://t.me/BotFather), send `/newbot`,
follow the prompts, and keep the token it gives you. (BotFather is only used
this once — after that, telepager talks to `api.telegram.org` directly.)

**2. Get your user ID.** Message [@userinfobot](https://t.me/userinfobot); it
replies with your numeric ID.

**3. Send your bot a message** so a chat exists — a bot can't open one with you.

**4. Write the config** at `~/.config/telepager/config.json`:

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

**5. Register it with your MCP client.** For Claude Code:

```bash
claude mcp add telepager -- telepager
```

Or by hand:

```json
{
  "mcpServers": {
    "telepager": {
      "command": "telepager"
    }
  }
}
```

Your client starts and stops the server itself — there's no daemon to babysit.

## Install

Build it:

```bash
cargo build --release   # -> target/release/telepager
```

Then put it somewhere on your PATH, or point your MCP client at the full path.
Prebuilt binaries for macOS and Linux (arm64/x64) are attached to each
[release](https://github.com/fordaaaa/telepager/releases).

## Security

The allowlist is the entire security model, so telepager **refuses to start
with an empty one**. Beyond that:

- **The token is a secret.** Anyone holding it can act as your bot. Prefer
  `TELEGRAM_BOT_TOKEN` over the plaintext file; the config file is gitignored.
- **Telegram is not end-to-end encrypted.** Its servers can see the traffic —
  don't have your agent page you with real secrets.

telepager cannot run commands, read your files, or touch your machine. It only
sends and receives Telegram messages.

## License

MIT
