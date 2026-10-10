# gray-telegram

A standalone [gray](https://github.com/vstaln/gray) channel adapter for the
Telegram Bot API. It long-polls `getUpdates` (no webhook), admits only the
owner and allowlisted users, runs each inbound message as an isolated
`gray -p --json` turn, and exposes two outbound tools
(`telegram_send`, `telegram_file`) to the agent through the gray sidecar
protocol.

It mirrors the gray Discord plugin's architecture — durable SQLite queue,
isolated per-conversation homes, pairing codes, budget/limits, service
lifecycle — on a `reqwest`-only transport (no teloxide, no MTProto).

## Install

```sh
cargo build --release
# binary: target/release/gray-telegram
gray plugin check ./target/release/gray-telegram   # conformance check
```

## Setup

```sh
gray-telegram setup            # token + your user id + home chat
gray-telegram setup --pair     # same, but discovers your ids via a
                               # one-time code you DM the bot
```

The token comes from `@BotFather`, then from a hidden prompt or the
`TELEGRAM_BOT_TOKEN` environment variable. Config lives at
`~/.config/gray-telegram/config.json` (0600); use `--config <path>` to
place it elsewhere.

A stranger who DMs the bot gets a one-shot pairing code; approve it with
`gray-telegram pairing approve telegram <code>` (the first approval becomes
the owner).

## Run

```sh
gray-telegram run          # foreground
gray-telegram install      # runit / systemd --user / detached fallback
gray-telegram status|stop|restart|uninstall
gray-telegram doctor       # gray binary, provider, getMe, home chat
gray-telegram register     # register the sidecar tools with gray
```

Useful commands: `queue list|cancel`, `schedule add|list|remove`,
`allowlist add|remove|list`, `limits`, `budget status|set`, `share`.

## In-chat commands

`/start` (pairing/greeting), `/new` (fresh session), `/stop` (cancel the
running turn), `/status`, `/help`. Photos and documents are downloaded via
`getFile` and handed to the agent as file paths.

## Tools

- `telegram_send` — HTML text (`<b>`, `<i>`, `<code>`, `<pre>`, `<a href>`,
  `<tg-spoiler>`), chunked at 4096 chars. A `MEDIA:/absolute/path` line
  attaches files.
- `telegram_file` — local files (`path` or `paths`, optional `caption`);
  images go through `sendPhoto`, the rest `sendDocument`. Credential and
  config paths are refused.

## Limitations

- Long polling only; no webhook mode.
- No voice notes, video notes, inline keyboards/buttons, topics, or
  message edits.
- Group chats: the bot answers only allowlisted users who mention it or
  reply to it (Telegram privacy mode means it may not even see other
  messages).
- Only the largest size of a photo is downloaded; inbound files are capped
  (default 8 MiB, `max_attachment_bytes`).
- Each Telegram chat/user pair is one conversation; concurrent turns for
  the same conversation are serialized through the durable queue.
