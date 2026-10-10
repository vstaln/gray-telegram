# gray-telegram runtime

## Layout

Everything lives under the config directory (`--config <path>`, default
`~/.config/gray-telegram/config.json`):

| path | purpose |
|------|---------|
| `config.json` | token, `chat_id`, `owner_id`, `allowed_users`, limits (0600) |
| `queue.sqlite` | durable inbox/outbox/schedules/meta/pairings |
| `daemon.json` | live pid + start time while the gateway runs |
| `state.json` | last known bot identity (`bot`, `bot_id`, `connected_at`) |
| `lifecycle.json` | `running`/`stopped` marker for crash vs clean boot notices |
| `gateway.lock` | `flock` guard against a second daemon |
| `conversations/<sha>/` | per-conversation gray home (session + workdir) |
| `work/attachments/` | downloaded inbound photos/documents |
| `gray-telegram.{pid,log}` | pidfile + log when no init supervises |

## Data flow

1. `run` long-polls `getUpdates` (30s long poll, reconnect backoff
   1s→60s, `retry_after` honored). The offset is persisted in the `meta`
   table after every batch; `deleteWebhook` at boot clears a stale webhook
   that would otherwise swallow updates.
2. Each message is admitted by policy (owner/allowlist, private vs.
   mention/reply-in-group), burst-checked, files downloaded via
   `getFile`+download, then enqueued as inbox row `tg:<update_id>`
   (dedupe-safe on replay).
3. `concurrency` generator workers `claim` rows — a conversation never has
   two running turns — spawn `gray -p --json` in an isolated home, stream
   NDJSON, and `complete`/`fail` the row. `MEDIA:` lines in the answer are
   uploaded as photos/documents.
4. A delivery worker sends outbox parts via `sendMessage`
   (`parse_mode=HTML`, 4096-char chunks); failures retry without
   re-running the agent. A schedule ticker enqueues due jobs; a pruner
   drops terminal rows after 7 days.
5. SIGTERM/SIGINT produce lifecycle notices ("restarting" vs "shutting
   down") in affected chats and the home chat.

## Sidecar protocol

Bare `gray-telegram` (or `gray-telegram sidecar`) speaks gray's NDJSON
protocol 1.1 over stdio: `plugin/manifest`, `prompt/context`,
`tool/call` (`telegram_send`, `telegram_file`), `plugin/shutdown`.
Requests carry numeric ids; replies are `{"id", "result"}`. Tools deliver
to the configured home chat; they need a valid config — delivery errors
never echo the token.

## Failure modes

- Crash mid-turn → `recover()` marks running rows `uncertain`; the next
  boot notice says how many turns were cut short.
- Bad token / Telegram 401 → the daemon exits so the supervisor surfaces it.
- 429 → the part sleeps `retry_after` and retries; the row stays queued.
- Full queue / invalid message → the sender is told it was not accepted.

## No live testing without credentials

`cargo build` and `gray plugin check` need no token. Everything past that —
`setup`, `doctor`, `run`, pairing, `telegram_send`/`telegram_file` —
needs a real `@BotFather` token and a Telegram account to DM the bot.
