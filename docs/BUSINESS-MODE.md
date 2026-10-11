# Draft-approval secretary (ported design from hermes `telegram-business`)

Source studied: `reference/NousResearch/hermes-official-plugins/telegram-business/`
(~1,360 lines: `manager.py` 766, `state.py` 302, `__init__.py` 291).
This doc is the gray port sketch — what the design is, what maps onto what
gray-telegram already has, and what's missing.

## The hermes flow

1. Owner connects the bot via BotFather Business Mode → bot receives a
   `BusinessConnection` update, persists it, DMs the owner an onboarding
   message.
2. Customer messages the owner in a covered chat → plugin debounces
   `debounce_seconds` (default 8s) to coalesce typing bursts → drafts ONE
   reply from the latest customer text → DMs the draft to the *owner's* DM
   with inline buttons `[✓ Send] [✎ Edit] [✕ Discard]`.
3. Send → message goes out under `business_connection_id`; Edit → owner's
   next DM text becomes the reply; Discard → dropped.
4. **No auto-send exists.** Even when Telegram grants `can_reply`, every
   send is an explicit owner tap. `/biz`, `/biz pause` plus per-chat
   `paused_chats` suspend drafting.

## What gray-telegram already has

- Inbound long-poll `getUpdates` with persisted offset (`update_offset`)
  and inbox dedup by `tg:<update_id>` — the trigger surface exists.
- A service process already runs (runit `gray-discord`-style supervision
  for telegram too) — a draft loop can live there.
- `tool/before` with `deny|allow|modify` + `host.ask` — the approval gate
  is a host primitive, not something we have to build.

## What's missing (the port)

- **Business updates**: `inbound()` currently reads `update.message` only;
  Business Mode needs `business_connection` and `business_message` update
  types, plus `callback_query` for the inline buttons.
- **Draft queue**: plugin-owned SQLite (their `state.py` schema:
  `business_connections` + `business_drafts`) — same pattern we already
  use: own file, never touches host state. Suggest `durable.rs`-adjacent
  table under the plugin's own data dir.
- **Approval surface**: their UX is Telegram inline buttons
  (`bd:<choice>:<draft_id>` callback_data, 64-byte cap). Gray equivalent:
  draft lands in the owner's configured approval channel (DM or
  `host.ask`), and the customer send sits behind a `tool/before` modify
  hook so even a tool-level `telegram_send` to a watched chat is gated.
- **Debounce + draft model**: customer burst → one draft; hold the latest
  text, drop superseded drafts. 8s default.
- **Pause**: `/biz` maps to a `tg biz` subcommand or a tool
  (`telegram_biz status|pause|resume`).

## Suggested tool surface

| tool | does |
|---|---|
| `tg_biz_status` | connections, pending drafts, paused chats |
| `tg_biz_pause` / `tg_biz_resume` | global or `--chat <id>` |
| `tg_biz_drafts` | pending queue with ids |
| `tg_biz_resolve` | `send|edit|discard` a draft id — the non-Telegram-button path |

## Rule that must survive the port

Nothing reaches a customer without an explicit owner approval, regardless
of Telegram-side capabilities. The draft is a proposal, not a send.
