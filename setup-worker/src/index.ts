import { DurableObject } from "cloudflare:workers";

export interface Env {
  BOT_TOKEN: string;
  WEBHOOK_SECRET: string;
  MANAGER_USERNAME: string;
  SETUP: DurableObjectNamespace<SetupStore>;
}

const PAIRING_TTL_MS = 10 * 60 * 1000;
const HOLD_MAX_MS = 25 * 1000;
const EXPIRY_GRACE_MS = HOLD_MAX_MS;
const RATE_WINDOW_MS = 10 * 60 * 1000;
const RATE_LIMIT = 10;
const USERNAME_ALPHABET = "abcdefghijklmnopqrstuvwxyz0123456789";
const TEXT_HINT = "Run `gray telegram setup` on your computer to get a setup link.";
const TEXT_KEYBOARD = "Tap below to create your Gray bot.";
const TEXT_BUTTON = "Create my Gray bot";
const TEXT_DONE = "Done. Go back to your terminal.";

type PairingStatus = "waiting" | "opened" | "ready";

type PairingBase = {
  pairing_id: string;
  poll_token_hash: string;
  suggested_username: string;
  expires_at: number;
};

type Pairing = PairingBase &
  (
    | { status: "waiting" }
    | { status: "opened"; owner_id: number; request_id: number | null }
    | { status: "ready"; owner_id: number; bot_token: string; bot_id: number; bot_username: string }
  );

type PairingRow = {
  pairing_id: string;
  poll_token_hash: string;
  suggested_username: string;
  request_id: number | null;
  owner_id: number | null;
  status: string;
  bot_token: string | null;
  bot_id: number | null;
  bot_username: string | null;
  expires_at: number;
  created_at: number;
};

type TelegramUser = { id: number; username?: string };

type TelegramUpdate = {
  message?: {
    chat?: { id: number };
    from?: TelegramUser;
    text?: string;
    managed_bot_created?: { bot: TelegramUser; request_id?: number };
  };
  managed_bot?: { user: TelegramUser; bot: TelegramUser; request_id?: number };
};

type CreatePairingResult =
  | {
      kind: "created";
      pairing_id: string;
      poll_token: string;
      suggested_username: string;
      deep_link: string;
      qr_payload: string;
      expires_in: number;
    }
  | { kind: "rate_limited"; retry_after: number };

type PollPairingResult =
  | { kind: "unknown" }
  | { kind: "bad_token" }
  | { kind: "expired" }
  | { kind: "waiting" }
  | { kind: "opened" }
  | { kind: "ready"; token: string; owner_id: number; bot_username: string; bot_id: number };

const json = (body: unknown, status: number): Response =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });

const randomBytes = (length: number): Uint8Array => {
  const buffer = new Uint8Array(length);
  crypto.getRandomValues(buffer);
  return buffer;
};

const hex = (bytes: Uint8Array): string =>
  Array.from(bytes)
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");

const base64Url = (bytes: Uint8Array): string =>
  btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

const randomPairingId = (): string => base64Url(randomBytes(12));

const randomPollToken = (): string => hex(randomBytes(32));

const randomRequestId = (): number => {
  const buffer = new Int32Array(1);
  crypto.getRandomValues(buffer);
  return buffer[0];
};

const randomSuggestedUsername = (): string => {
  let suffix = "";
  for (const byte of randomBytes(6)) suffix += USERNAME_ALPHABET[byte % USERNAME_ALPHABET.length];
  return `gray_${suffix}_bot`;
};

const sha256Hex = async (value: string): Promise<string> =>
  hex(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value))));

const timingSafeEqual = (left: string, right: string): boolean => {
  const a = new TextEncoder().encode(left);
  const b = new TextEncoder().encode(right);
  let diff = a.length ^ b.length;
  for (let i = 0; i < Math.max(a.length, b.length); i += 1) diff |= (a[i] ?? 0) ^ (b[i] ?? 0);
  return diff === 0;
};

const parsePairing = (row: PairingRow): Pairing => {
  const base: PairingBase = {
    pairing_id: row.pairing_id,
    poll_token_hash: row.poll_token_hash,
    suggested_username: row.suggested_username,
    expires_at: row.expires_at,
  };
  switch (row.status as PairingStatus) {
    case "waiting":
      return { ...base, status: "waiting" };
    case "opened":
      if (row.owner_id === null) throw new Error(`pairing ${row.pairing_id} is opened without an owner`);
      return { ...base, status: "opened", owner_id: row.owner_id, request_id: row.request_id };
    case "ready":
      if (row.owner_id === null || row.bot_token === null || row.bot_id === null) {
        throw new Error(`pairing ${row.pairing_id} is ready without bot credentials`);
      }
      return {
        ...base,
        status: "ready",
        owner_id: row.owner_id,
        bot_token: row.bot_token,
        bot_id: row.bot_id,
        bot_username: row.bot_username ?? "",
      };
    default:
      throw new Error(`pairing ${row.pairing_id} has unknown status ${row.status}`);
  }
};

const telegram = async (env: Env, method: string, payload: Record<string, unknown>): Promise<unknown> => {
  let response: Response;
  try {
    response = await fetch(`https://api.telegram.org/bot${env.BOT_TOKEN}/${method}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(payload),
    });
  } catch {
    throw new Error(`${method} request failed`);
  }
  const body = (await response.json().catch(() => null)) as
    | { ok?: boolean; result?: unknown; description?: string }
    | null;
  if (body === null || body.ok !== true) {
    throw new Error(`${method} failed: ${body?.description ?? `http ${response.status}`}`);
  }
  return body.result;
};

export class SetupStore extends DurableObject<Env> {
  private readonly waiters = new Map<string, Set<() => void>>();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.storage.sql.exec(
      `CREATE TABLE IF NOT EXISTS pairings (
        pairing_id TEXT PRIMARY KEY,
        poll_token_hash TEXT NOT NULL,
        suggested_username TEXT NOT NULL,
        request_id INTEGER,
        owner_id INTEGER,
        status TEXT NOT NULL,
        bot_token TEXT,
        bot_id INTEGER,
        bot_username TEXT,
        expires_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL
      )`,
    );
    ctx.storage.sql.exec(
      `CREATE TABLE IF NOT EXISTS rate_limits (
        ip TEXT PRIMARY KEY,
        window_start INTEGER NOT NULL,
        count INTEGER NOT NULL
      )`,
    );
  }

  async alarm(): Promise<void> {
    const now = Date.now();
    const sql = this.ctx.storage.sql;
    sql.exec("DELETE FROM pairings WHERE expires_at <= ?", now - EXPIRY_GRACE_MS);
    sql.exec("DELETE FROM rate_limits WHERE window_start < ?", now - RATE_WINDOW_MS);
    const pending = sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM pairings").one().n;
    if (pending > 0) await this.ctx.storage.setAlarm(now + PAIRING_TTL_MS);
  }

  async createPairing(ip: string): Promise<CreatePairingResult> {
    const now = Date.now();
    const sql = this.ctx.storage.sql;
    const windowStart = Math.floor(now / RATE_WINDOW_MS) * RATE_WINDOW_MS;
    const limit = sql
      .exec<{ window_start: number; count: number }>(
        "SELECT window_start, count FROM rate_limits WHERE ip = ?",
        ip,
      )
      .toArray()[0];

    if (limit === undefined || limit.window_start !== windowStart) {
      sql.exec("INSERT OR REPLACE INTO rate_limits (ip, window_start, count) VALUES (?, ?, 1)", ip, windowStart);
    } else if (limit.count >= RATE_LIMIT) {
      return { kind: "rate_limited", retry_after: Math.ceil((windowStart + RATE_WINDOW_MS - now) / 1000) };
    } else {
      sql.exec("UPDATE rate_limits SET count = count + 1 WHERE ip = ?", ip);
    }

    const pairingId = randomPairingId();
    const pollToken = randomPollToken();
    const suggestedUsername = randomSuggestedUsername();
    const expiresAt = now + PAIRING_TTL_MS;

    sql.exec(
      `INSERT INTO pairings
        (pairing_id, poll_token_hash, suggested_username, request_id, owner_id, status, bot_token, bot_id, bot_username, expires_at, created_at)
        VALUES (?, ?, ?, NULL, NULL, 'waiting', NULL, NULL, NULL, ?, ?)`,
      pairingId,
      await sha256Hex(pollToken),
      suggestedUsername,
      expiresAt,
      now,
    );

    if ((await this.ctx.storage.getAlarm()) === null) {
      await this.ctx.storage.setAlarm(expiresAt);
    }

    const deepLink = `https://t.me/${this.env.MANAGER_USERNAME}?start=${pairingId}`;
    return {
      kind: "created",
      pairing_id: pairingId,
      poll_token: pollToken,
      suggested_username: suggestedUsername,
      deep_link: deepLink,
      qr_payload: deepLink,
      expires_in: Math.round(PAIRING_TTL_MS / 1000),
    };
  }

  async pollPairing(pairingId: string, pollToken: string, holdMs: number): Promise<PollPairingResult> {
    const tokenHash = await sha256Hex(pollToken);
    const current = this.readPoll(pairingId, tokenHash);
    switch (current.kind) {
      case "unknown":
      case "bad_token":
      case "expired":
      case "ready":
        return current;
      case "waiting":
      case "opened":
        await this.waitForChange(pairingId, holdMs);
        return this.readPoll(pairingId, tokenHash);
    }
  }

  private readPoll(pairingId: string, tokenHash: string): PollPairingResult {
    const sql = this.ctx.storage.sql;
    const row = sql.exec<PairingRow>("SELECT * FROM pairings WHERE pairing_id = ?", pairingId).toArray()[0];
    if (row === undefined) return { kind: "unknown" };
    if (!timingSafeEqual(tokenHash, row.poll_token_hash)) return { kind: "bad_token" };
    if (row.expires_at <= Date.now()) return { kind: "expired" };

    const pairing = parsePairing(row);
    switch (pairing.status) {
      case "waiting":
        return { kind: "waiting" };
      case "opened":
        return { kind: "opened" };
      case "ready": {
        if (sql.exec("DELETE FROM pairings WHERE pairing_id = ?", pairing.pairing_id).rowsWritten === 0) {
          return { kind: "unknown" };
        }
        return {
          kind: "ready",
          token: pairing.bot_token,
          owner_id: pairing.owner_id,
          bot_username: pairing.bot_username,
          bot_id: pairing.bot_id,
        };
      }
    }
  }

  private notifyWaiters(pairingId: string): void {
    const waiters = this.waiters.get(pairingId);
    if (waiters === undefined) return;
    this.waiters.delete(pairingId);
    for (const wake of waiters) wake();
  }

  private waitForChange(pairingId: string, holdMs: number): Promise<void> {
    return new Promise<void>((resolve) => {
      const waiters = this.waiters.get(pairingId) ?? new Set<() => void>();
      this.waiters.set(pairingId, waiters);
      let timer: ReturnType<typeof setTimeout> | null = null;
      const wake = (): void => {
        if (timer !== null) clearTimeout(timer);
        waiters.delete(wake);
        if (this.waiters.get(pairingId) === waiters) this.waiters.delete(pairingId);
        resolve();
      };
      timer = setTimeout(wake, holdMs);
      waiters.add(wake);
    });
  }

  async handleUpdate(update: TelegramUpdate): Promise<void> {
    const managed = update.managed_bot;
    if (managed) {
      await this.onBotCreated(managed.user.id, managed.bot, managed.request_id ?? null);
      return;
    }

    const message = update.message;
    const from = message?.from;
    const chat = message?.chat;
    if (!message || !from || !chat) return;

    const created = message.managed_bot_created;
    if (created) {
      await this.onBotCreated(from.id, created.bot, created.request_id ?? null);
      return;
    }

    const text = message.text;
    if (typeof text !== "string" || !text.startsWith("/start")) return;
    const payload = text.split(/\s+/)[1] ?? "";
    if (payload === "") {
      await this.send(chat.id, TEXT_HINT);
      return;
    }
    await this.onStart(chat.id, from.id, payload);
  }

  private async onStart(chatId: number, userId: number, pairingId: string): Promise<void> {
    const sql = this.ctx.storage.sql;
    const row = sql.exec<PairingRow>("SELECT * FROM pairings WHERE pairing_id = ?", pairingId).toArray()[0];
    if (row === undefined || row.expires_at <= Date.now()) {
      await this.send(chatId, TEXT_HINT);
      return;
    }

    const pairing = parsePairing(row);
    switch (pairing.status) {
      case "waiting": {
        const requestId = randomRequestId();
        sql.exec(
          "UPDATE pairings SET status = 'opened', owner_id = ?, request_id = ? WHERE pairing_id = ?",
          userId,
          requestId,
          pairingId,
        );
        this.notifyWaiters(pairingId);
        await this.sendCreateKeyboard(chatId, pairing.suggested_username, requestId);
        return;
      }
      case "opened": {
        if (pairing.owner_id !== userId || pairing.request_id === null) {
          await this.send(chatId, TEXT_HINT);
          return;
        }
        await this.sendCreateKeyboard(chatId, pairing.suggested_username, pairing.request_id);
        return;
      }
      case "ready": {
        await this.send(chatId, TEXT_HINT);
        return;
      }
    }
  }

  private async onBotCreated(ownerId: number, bot: TelegramUser, requestId: number | null): Promise<void> {
    const sql = this.ctx.storage.sql;
    const now = Date.now();
    const row = (
      requestId === null
        ? sql
            .exec<PairingRow>(
              "SELECT * FROM pairings WHERE status = 'opened' AND owner_id = ? AND expires_at > ? ORDER BY created_at DESC",
              ownerId,
              now,
            )
            .toArray()
        : sql
            .exec<PairingRow>(
              "SELECT * FROM pairings WHERE status = 'opened' AND owner_id = ? AND request_id = ? AND expires_at > ? ORDER BY created_at DESC",
              ownerId,
              requestId,
              now,
            )
            .toArray()
    )[0];

    if (row === undefined) {
      console.log(`managed bot ${bot.id} has no open pairing for user ${ownerId}`);
      return;
    }

    let token: string;
    try {
      token = String(await telegram(this.env, "getManagedBotToken", { user_id: bot.id }));
    } catch (error) {
      console.log(`managed bot ${bot.id} token lookup failed: ${String(error)}`);
      return;
    }

    sql.exec(
      "UPDATE pairings SET status = 'ready', bot_token = ?, bot_id = ?, bot_username = ? WHERE pairing_id = ?",
      token,
      bot.id,
      bot.username ?? "",
      row.pairing_id,
    );
    this.notifyWaiters(row.pairing_id);

    try {
      await telegram(this.env, "sendMessage", {
        chat_id: ownerId,
        text: TEXT_DONE,
        reply_markup: { remove_keyboard: true },
      });
    } catch (error) {
      console.log(`managed bot ${bot.id} confirmation failed: ${String(error)}`);
    }
  }

  private async send(chatId: number, text: string): Promise<void> {
    await telegram(this.env, "sendMessage", { chat_id: chatId, text });
  }

  private async sendCreateKeyboard(chatId: number, suggestedUsername: string, requestId: number): Promise<void> {
    await telegram(this.env, "sendMessage", {
      chat_id: chatId,
      text: TEXT_KEYBOARD,
      reply_markup: {
        keyboard: [
          [
            {
              text: TEXT_BUTTON,
              request_managed_bot: {
                request_id: requestId,
                suggested_name: "Gray",
                suggested_username: suggestedUsername,
              },
            },
          ],
        ],
        resize_keyboard: true,
        one_time_keyboard: true,
      },
    });
  }
}

const createPairing = async (request: Request, env: Env): Promise<Response> => {
  const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
  const store = env.SETUP.get(env.SETUP.idFromName("setup"));
  const result = await store.createPairing(ip);
  switch (result.kind) {
    case "created":
      return json(
        {
          pairing_id: result.pairing_id,
          poll_token: result.poll_token,
          suggested_username: result.suggested_username,
          deep_link: result.deep_link,
          qr_payload: result.qr_payload,
          expires_in: result.expires_in,
        },
        201,
      );
    case "rate_limited":
      return new Response(JSON.stringify({ error: "rate_limited" }), {
        status: 429,
        headers: { "content-type": "application/json", "cache-control": "no-store", "retry-after": String(result.retry_after) },
      });
  }
};

const pollPairing = async (request: Request, env: Env, pairingId: string): Promise<Response> => {
  const header = request.headers.get("authorization") ?? "";
  const token = header.toLowerCase().startsWith("bearer ") ? header.slice(7).trim() : "";
  if (token === "") return json({ error: "unauthorized" }, 401);

  const store = env.SETUP.get(env.SETUP.idFromName("setup"));
  const result = await store.pollPairing(pairingId, token, HOLD_MAX_MS);
  switch (result.kind) {
    case "unknown":
      return json({ error: "not_found" }, 404);
    case "bad_token":
      return json({ error: "unauthorized" }, 401);
    case "expired":
      return json({ error: "expired" }, 410);
    case "waiting":
      return json({ status: "waiting" }, 202);
    case "opened":
      return json({ status: "opened" }, 202);
    case "ready":
      return json(
        {
          status: "ready",
          token: result.token,
          owner_id: result.owner_id,
          bot_username: result.bot_username,
          bot_id: result.bot_id,
        },
        200,
      );
  }
};

const webhook = async (request: Request, env: Env): Promise<Response> => {
  const provided = request.headers.get("x-telegram-bot-api-secret-token") ?? "";
  if (env.WEBHOOK_SECRET === undefined || !timingSafeEqual(provided, env.WEBHOOK_SECRET)) {
    return json({ error: "unauthorized" }, 401);
  }

  let update: TelegramUpdate;
  try {
    update = (await request.json()) as TelegramUpdate;
  } catch (error) {
    console.log(`webhook body rejected: ${String(error)}`);
    return json({ ok: true }, 200);
  }

  try {
    await env.SETUP.get(env.SETUP.idFromName("setup")).handleUpdate(update);
  } catch (error) {
    console.log(`webhook handling failed: ${String(error)}`);
  }
  return json({ ok: true }, 200);
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/v1/pairings") {
      if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
      return createPairing(request, env);
    }
    const poll = url.pathname.match(/^\/v1\/pairings\/([A-Za-z0-9_-]{16})$/);
    if (poll !== null) {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      return pollPairing(request, env, poll[1]);
    }
    if (url.pathname === "/telegram/webhook") {
      if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
      return webhook(request, env);
    }
    return json({ error: "not_found" }, 404);
  },
} satisfies ExportedHandler<Env>;
