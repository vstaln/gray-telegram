//! Owner/allowlist-only gateway with durable generation and delivery
//! workers. Inbound is Telegram long-poll `getUpdates`: the offset is
//! persisted after each batch so a restart replays at most the last batch,
//! and the inbox row id (`tg:<update_id>`) dedupes that replay. There is no
//! webhook — `deleteWebhook` at boot keeps a stale one from eating updates.

use crate::durable::{OutboxPart, Store};
use crate::runner::{RunError, RunInput};
use crate::transport::{Rest, TransportError};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The state file `gray`'s gateway row reads to tell a live daemon from a
/// stale one.
fn daemon_state_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("daemon.json")
}

/// Where the persistent getUpdates offset lives.
const OFFSET_KEY: &str = "update_offset";
const BUSY_BACKOFF_SECS: f64 = 3.0;
const QUEUED_ACK_SECS: f64 = 30.0;

fn busy_input_steer(config: &Value) -> bool {
    config.get("busy_input_mode").and_then(Value::as_str) == Some("steer")
}

pub fn queued_notice(store: &Store, conversation: &str, id: &str) -> Option<String> {
    static ACKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, f64>>> =
        std::sync::OnceLock::new();
    let ahead = store.ahead_in_conversation(conversation, id).ok()?;
    if ahead <= 0 {
        return None;
    }
    let now = crate::durable::now_secs();
    let mut acks = ACKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if acks
        .get(conversation)
        .is_some_and(|at| now - at < QUEUED_ACK_SECS)
    {
        return None;
    }
    acks.retain(|_, at| now - *at < QUEUED_ACK_SECS);
    acks.insert(conversation.to_string(), now);
    Some(format!("Queued — {ahead} ahead."))
}

/// Queue/listing helper used by the CLI (`queue`, `schedule`, `pairing`).
pub fn open_store(config_path: &Path) -> Result<Store, String> {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    Store::new(&parent.join("queue.sqlite"))
}

/// Reconnect backoff after a poll failure: 1s doubling to a minute.
fn poll_backoff(attempt: u64) -> std::time::Duration {
    let secs = (1u64 << attempt.min(6)).min(60);
    std::time::Duration::from_secs(secs)
}

/// A normalized inbound message: everything the admission path needs.
struct Inbound {
    update_id: i64,
    chat_id: String,
    is_private: bool,
    user_id: String,
    is_bot: bool,
    text: String,
    /// `(file_id, suggested filename)` pairs from photo/document payloads.
    files: Vec<(String, String)>,
    mentioned: bool,
    reply_to_bot: bool,
}

/// Pull one inbound message out of an update. Channels post as
/// `sender_chat` without a `from` user — they cannot pair or be admitted.
fn inbound(update: &Value, bot_username: &str, bot_id: &str) -> Option<Inbound> {
    let msg = update.get("message")?;
    let update_id = update.get("update_id").and_then(Value::as_i64)?;
    let chat = msg.get("chat")?;
    let chat_id = chat.get("id").and_then(Value::as_i64)?.to_string();
    let chat_type = chat.get("type").and_then(Value::as_str).unwrap_or("");
    let is_private = chat_type == "private";
    let from = msg.get("from").cloned().unwrap_or(Value::Null);
    let user_id = from.get("id").and_then(Value::as_i64)?.to_string();
    let is_bot = from.get("is_bot").and_then(Value::as_bool).unwrap_or(false);
    let mut text = msg
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| msg.get("caption").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let mut files = Vec::new();
    if let Some(photos) = msg.get("photo").and_then(Value::as_array) {
        // PhotoSize rows arrive smallest→largest; take the biggest.
        let best = photos
            .iter()
            .filter_map(|p| {
                let id = p.get("file_id").and_then(Value::as_str)?;
                let w = p.get("width").and_then(Value::as_u64).unwrap_or(0);
                Some((id, w))
            })
            .max_by_key(|(_, w)| *w);
        if let Some((id, _)) = best {
            files.push((id.to_string(), "photo.jpg".to_string()));
        }
    }
    if let Some(doc) = msg.get("document") {
        if let Some(id) = doc.get("file_id").and_then(Value::as_str) {
            let name = doc
                .get("file_name")
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_string();
            files.push((id.to_string(), name));
        }
    }
    // Mentions and commands are `entities` offsets; a bot mention is any
    // mention of our own username anywhere in the text.
    let mentioned = msg
        .get("entities")
        .or_else(|| msg.get("caption_entities"))
        .and_then(Value::as_array)
        .map(|entities| {
            entities.iter().any(|e| {
                if e.get("type").and_then(Value::as_str) != Some("mention") {
                    return false;
                }
                let (offset, len) = (
                    e.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
                    e.get("length").and_then(Value::as_u64).unwrap_or(0) as usize,
                );
                text.get(offset..offset + len)
                    .map(|m| m.eq_ignore_ascii_case(&format!("@{bot_username}")))
                    .unwrap_or(false)
            })
        })
        .unwrap_or_else(|| {
            text.to_ascii_lowercase()
                .contains(&format!("@{}", bot_username.to_ascii_lowercase()))
        });
    let reply_to_bot = msg
        .get("reply_to_message")
        .and_then(|r| r.get("from"))
        .and_then(|f| f.get("id"))
        .and_then(Value::as_i64)
        .is_some_and(|id| id.to_string() == bot_id);
    // A caption-only message with a file counts as an attachment turn even
    // with empty text; a bare command stays a command.
    if text.trim().is_empty() && files.is_empty() {
        return None;
    }
    Some(Inbound {
        update_id,
        chat_id,
        is_private,
        user_id,
        is_bot,
        // Telegram text offsets are UTF-16 but mention detection above is
        // also done by plain substring fallback; keep the raw text.
        text: std::mem::take(&mut text),
        files,
        mentioned,
        reply_to_bot,
    })
}

/// One line on the daemon's stderr for a message that did not land. Never
/// message content.
fn log_failure(what: &str, chat: &str, error: &TransportError) {
    let detail = match error {
        TransportError::Http(code, detail) => format!("HTTP {code}: {detail}"),
        TransportError::Net(detail) => format!("network: {detail}"),
        TransportError::RateLimited(after) => format!("rate limited, retry after {after:?}s"),
        TransportError::Invalid(detail) => format!("not sent: {detail}"),
        TransportError::Auth(_) => "unauthorized".to_string(),
        TransportError::Forbidden(_) => "forbidden".to_string(),
    };
    eprintln!("[telegram] {what} failed in chat {chat}: {detail}");
}

#[derive(Clone)]
pub struct Runtime<R, D> {
    pub config: Value,
    pub config_path: PathBuf,
    pub store: Store,
    pub deliver: D,
    pub runner: R,
    pub rest: Option<Rest>,
    pub clock: Option<std::sync::Arc<dyn Fn() -> f64 + Send + Sync>>,
    last_typing: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, f64>>>,
    busy_backoff: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
}

impl<R, D> Runtime<R, D> {
    pub fn new(config: Value, config_path: PathBuf, store: Store, deliver: D, runner: R) -> Self {
        Self {
            config,
            config_path,
            store,
            deliver,
            runner,
            rest: None,
            clock: None,
            last_typing: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            busy_backoff: Default::default(),
        }
    }

    pub fn with_rest(mut self, rest: Rest) -> Self {
        self.rest = Some(rest);
        self
    }

    pub fn with_clock(mut self, clock: std::sync::Arc<dyn Fn() -> f64 + Send + Sync>) -> Self {
        self.clock = Some(clock);
        self
    }

    pub fn now_secs(&self) -> f64 {
        self.clock
            .as_ref()
            .map(|c| c())
            .unwrap_or_else(crate::durable::now_secs)
    }

    /// The Telegram typing indicator, on by default. sendChatAction lasts
    /// ~5s; the claim path re-pokes it at most every 4s while a turn runs.
    pub fn typing_enabled(&self) -> bool {
        self.config
            .get("typing_indicator")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    pub async fn report_progress(&self, channel: &str) {
        if !self.typing_enabled() {
            return;
        }
        let now = self.now_secs();
        let mut map = self.last_typing.lock().await;
        let last = map.get(channel).copied().unwrap_or(-10.0);
        if now - last >= 4.0 {
            map.insert(channel.to_string(), now);
            drop(map);
            if let Some(ref rest) = self.rest {
                let _ = rest.typing(channel).await;
            }
        }
    }

    /// Where a relative `MEDIA:` path resolves: the configured workdir.
    fn media_cwd(&self) -> PathBuf {
        PathBuf::from(
            self.config
                .get("workdir")
                .and_then(Value::as_str)
                .unwrap_or("."),
        )
    }

    fn media_roots(&self) -> Vec<PathBuf> {
        crate::media_tags::roots_from_config(&self.config)
    }

    /// The gray home for one conversation (the same layout `run_gray`
    /// builds), or `None` when the layout cannot be derived.
    pub fn conversation_home(&self, conversation: &str) -> Option<PathBuf> {
        let base = self.config_path.parent()?;
        Some(
            base.join("conversations")
                .join(crate::runner::hex_sha256(conversation.as_bytes())),
        )
    }

    /// Send each MEDIA: file the answer named (photos as photos, the rest
    /// as documents). Uploads are best-effort: a file Telegram refuses is
    /// named in a follow-up text part so the user is not left guessing.
    async fn deliver_media(&self, channel: &str, media: &[PathBuf]) {
        let Some(ref rest) = self.rest else { return };
        for upload in crate::media_tags::load(media) {
            let send = if upload.is_photo() {
                rest.send_photo(channel, &upload.path, None).await
            } else {
                rest.send_document(channel, &upload.path, None).await
            };
            if let Err(error) = send {
                log_failure("file upload", channel, &error);
                let _ = rest
                    .send_text(
                        channel,
                        &crate::text::escape_html(&format!(
                            "[could not deliver file: {}]",
                            upload.name
                        )),
                    )
                    .await;
            }
        }
    }

    /// Run claimed turns until the queue drains. `Ok(false)` means "idle".
    pub async fn generate_one(&self) -> Result<bool, String>
    where
        R: Fn(
                &Value,
                &Path,
                &str,
                &RunInput,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, RunError>> + Send>,
            > + Send
            + Sync,
    {
        let skip: Vec<String> = {
            let now = self.now_secs();
            let mut backoff = self.busy_backoff.lock().unwrap_or_else(|e| e.into_inner());
            backoff.retain(|_, until| *until > now);
            backoff.keys().cloned().collect()
        };
        let item = match self.store.claim_except(&skip)? {
            Some(it) => it,
            None => return Ok(false),
        };
        self.report_progress(&item.channel).await;

        let run_fut = Box::pin((self.runner)(
            &self.config,
            &self.config_path,
            &item.conversation,
            &RunInput::Text(item.prompt.clone()),
        ));
        tokio::pin!(run_fut);

        // While the turn runs: re-poke typing (the bubble fades in ~5s) and
        // watch the cancel flag a `/stop` sets.
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(4));
        ticker.tick().await;
        let result = loop {
            tokio::select! {
                res = &mut run_fut => break Some(res),
                _ = ticker.tick() => {
                    self.report_progress(&item.channel).await;
                    if let Ok(Some(cur)) = self.store.get(&item.id) {
                        if cur.cancel {
                            break None;
                        }
                    }
                }
            }
        };
        let Some(result) = result else {
            self.store.fail(&item.id, "cancelled", "")?;
            return Ok(true);
        };
        match result {
            Ok(answer) => {
                let (prose, media) =
                    crate::media_tags::extract(&answer, &self.media_cwd(), &self.media_roots());
                let receipt = serde_json::json!({});
                if prose.trim().is_empty() {
                    self.store
                        .complete(&item.id, "(no text answer)", &receipt)?;
                } else {
                    self.store.complete(&item.id, &prose, &receipt)?;
                }
                self.deliver_media(&item.channel, &media).await;
            }
            Err(RunError::Busy) => {
                self.store.requeue(&item.id)?;
                let until = self.now_secs() + BUSY_BACKOFF_SECS;
                self.busy_backoff
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(item.conversation.clone(), until);
            }
            Err(RunError::Budget(e)) => {
                self.store
                    .fail(&item.id, "budget_blocked", &e.to_string())?
            }
            Err(e @ RunError::Timeout(_)) => {
                self.store.fail(&item.id, "timeout", &e.to_string())?
            }
            Err(e) => self.store.fail(&item.id, "agent_failed", &e.to_string())?,
        }
        Ok(true)
    }

    pub async fn deliver_one(&self) -> Result<bool, String>
    where
        D: Fn(
                OutboxPart,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
            > + Send
            + Sync,
    {
        let part = match self.store.next_delivery(self.now_secs())? {
            Some(p) => p,
            None => return Ok(false),
        };
        match (self.deliver)(part.clone()).await {
            Ok(msg_id) if !msg_id.trim().is_empty() => {
                self.store.ack(&part.id, part.part, &msg_id)?;
            }
            Ok(_) => {
                self.store
                    .delivery_failed(&part, "delivery_failed", self.now_secs())?;
            }
            Err(e) => {
                eprintln!(
                    "[telegram] delivery of {} part {} failed: {e}",
                    part.id, part.part
                );
                self.store
                    .delivery_failed(&part, "delivery_failed", self.now_secs())?;
            }
        }
        Ok(true)
    }
}

pub async fn run(config_path: &Path) -> Result<(), String> {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(parent)
        .map_err(|_| "cannot create config directory".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }

    // A pidfile next to the config: the gateway row reads it to tell a
    // connected daemon from a stale state file.
    let pid_path = daemon_state_path(config_path);
    {
        let stamp = serde_json::json!({
            "pid": std::process::id(),
            "started_at": crate::durable::now_secs()
        });
        let _ = crate::config::atomic_json(&pid_path, &stamp);
    }
    struct RemovePid(PathBuf);
    impl Drop for RemovePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_pid = RemovePid(pid_path);

    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(parent.join("gateway.lock"))
        .map_err(|_| "cannot open gateway lock".to_string())?;
    use std::os::unix::io::AsRawFd;
    let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Err("Another gateway is running".to_string());
    }
    let _lock = lock_file;

    let mut config = crate::config::load_config(config_path)?;
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .ok_or_else(|| "gray_home is missing from config".to_string())?;
    let provider_bytes = std::fs::read(Path::new(gray_home).join("config.json"))
        .map_err(|_| "gray provider configuration is missing".to_string())?;
    let provider: Value = serde_json::from_slice(&provider_bytes)
        .map_err(|_| "Invalid gray config.json".to_string())?;
    let model = provider
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "provider model is missing".to_string())?;
    let model = config
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(model);
    config["budget_required"] = Value::Bool(crate::budget::gate(&config, model)?);

    let store = open_store(config_path)?;
    let token = crate::config::token(&config).ok_or_else(|| "token missing".to_string())?;
    // Ownerless is a legal bootstrap state: nobody is admitted, and every
    // human DM draws a pairing reply until the operator approves a code.
    let owner_id = config
        .get("owner_id")
        .and_then(Value::as_str)
        .filter(|o| !o.is_empty())
        .unwrap_or_default()
        .to_string();
    let allowed: Vec<String> = config
        .get("allowed_users")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Sliding-window burst guard per user.
    let limiter = std::sync::Arc::new(std::sync::Mutex::new(crate::ratelimit::RateLimiter::new(
        config
            .get("rate_limit_capacity")
            .and_then(Value::as_u64)
            .unwrap_or(8) as usize,
        config
            .get("rate_limit_window_secs")
            .and_then(Value::as_u64)
            .unwrap_or(60),
    )));
    let workdir = config
        .get("workdir")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string();

    // The `gray cmd` registry (5-minute cache); `list` runs in the main home.
    let registry = std::sync::Arc::new(crate::graycmd::Registry::new(
        PathBuf::from(
            config
                .get("gray_bin")
                .and_then(Value::as_str)
                .unwrap_or("gray"),
        ),
        PathBuf::from(
            config
                .get("gray_home")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
    ));

    let rest = Rest::production(&token);
    // A stale webhook silently eats the updates we poll for; clear it.
    if let Err(e) = rest.delete_webhook().await {
        eprintln!("[telegram] deleteWebhook failed: {e}");
    }
    let me = rest.get_me().await.map_err(|e| e.to_string())?;
    let bot_id = me
        .get("id")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .to_string();
    let bot_username = me
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    {
        // Pin the identity for gray's gateway row: files only, no token.
        let state_path = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("state.json");
        let state = serde_json::json!({
            "bot": me.get("username").and_then(Value::as_str).unwrap_or(""),
            "bot_id": bot_id,
            "connected_at": crate::durable::now_secs()
        });
        let _ = crate::config::atomic_json(&state_path, &state);
    }
    // The visible slash menu: built-ins plus the top `cmd` entries.
    crate::graycmd::register_commands(&rest, &registry).await;

    // Boot lifecycle: count the turns a dead run left marked running BEFORE
    // recovery rewrites them, then recover them into `uncertain`.
    let running_before = store.running_channels().unwrap_or_default().len();
    let _ = store.recover();
    let dir = parent.to_path_buf();
    let gray_bin = PathBuf::from(
        config
            .get("gray_bin")
            .and_then(Value::as_str)
            .unwrap_or("gray"),
    );
    if crate::lifecycle::enabled(&config) {
        if let Some(notice) = crate::lifecycle::boot_via(&gray_bin, &dir, running_before).await {
            let home = config.get("chat_id").and_then(Value::as_str).unwrap_or("");
            if !home.is_empty() {
                let _ = rest
                    .send_text(home, &crate::text::escape_html(&notice))
                    .await;
            }
        }
    }

    let rest_del = rest.clone();
    let deliver = move |part: OutboxPart| {
        let rest = rest_del.clone();
        Box::pin(async move {
            let result = if let Some(path) = part.content.strip_prefix(crate::graycmd::FILE_PREFIX)
            {
                rest.send_document(&part.channel, Path::new(path), None)
                    .await
            } else {
                let text = crate::text::sanitize(&part.content);
                rest.send_text(&part.channel, &text).await
            };
            match result {
                Ok(v) => v
                    .get("message_id")
                    .and_then(Value::as_i64)
                    .map(|id| id.to_string())
                    .ok_or_else(|| "Delivery returned no message ID".to_string()),
                // Flood control is a wait, not a failure class of its own.
                Err(TransportError::RateLimited(after)) => {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(
                        after.unwrap_or(5.0).clamp(1.0, 60.0),
                    ))
                    .await;
                    Err("rate limited".to_string())
                }
                Err(e) => Err(e.to_string()),
            }
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
    };

    let runner = move |cfg: &Value, pth: &Path, conv: &str, input: &RunInput| {
        let cfg = cfg.clone();
        let pth = pth.to_path_buf();
        let conv = conv.to_string();
        let input = input.clone();
        Box::pin(async move {
            crate::runner::run_gray_input(&cfg, &pth, &conv, input, crate::runner::default_opts())
                .await
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, RunError>> + Send>>
    };

    let runtime = std::sync::Arc::new(
        Runtime::new(
            config.clone(),
            config_path.to_path_buf(),
            store.clone(),
            deliver,
            runner,
        )
        .with_rest(rest.clone()),
    );

    // Workers: `concurrency` generators, one delivery pump, one schedule
    // ticker. Any hard error ends the daemon so the supervisor restarts it.
    let concurrency = config
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(2) as usize;
    let home_channel = config
        .get("chat_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..concurrency.max(1) {
        let r = runtime.clone();
        set.spawn(async move {
            loop {
                match r.generate_one().await {
                    Ok(true) => {}
                    Ok(false) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
                    Err(e) => return Err(e),
                }
            }
        });
    }
    {
        let r = runtime.clone();
        set.spawn(async move {
            loop {
                match r.deliver_one().await {
                    Ok(true) => {}
                    Ok(false) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
                    Err(e) => return Err(e),
                }
            }
        });
    }
    {
        let ch = home_channel.clone();
        let st = store.clone();
        set.spawn(async move {
            loop {
                let _ = st.enqueue_due(&ch, crate::durable::now_secs());
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }
    // Prune terminal rows hourly.
    {
        let st = store.clone();
        set.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                let _ = st.prune_terminal(7 * 86400, crate::durable::now_secs());
            }
        });
    }

    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let sigterm_recv = async {
        #[cfg(unix)]
        if let Some(ref mut s) = sigterm {
            s.recv().await;
            return;
        }
        #[allow(unreachable_code)]
        std::future::pending::<()>().await
    };
    tokio::pin!(sigterm_recv);

    if owner_id.is_empty() {
        println!(
            "Telegram connected as @{bot_username}; nobody admitted yet (pairing replies only)."
        );
    } else {
        println!("Telegram connected as @{bot_username}; durable allowlisted queue enabled.");
    }

    // The poll loop. `offset` starts at the last confirmed update so a
    // restart never replays old work; each batch commits `max+1` after the
    // messages are handled (or skipped — a dropped update is also acked).
    let mut offset: i64 = store
        .meta_get(OFFSET_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut failures: u64 = 0;
    let max_attachment_bytes = config
        .get("max_attachment_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(8 * 1024 * 1024);

    'outer: loop {
        tokio::select! {
            _ = &mut sigterm_recv => break 'outer,
            _ = tokio::signal::ctrl_c() => break 'outer,
            res = set.join_next() => {
                // A worker died; stop the daemon so the supervisor restarts.
                set.abort_all();
                match res {
                    Some(Ok(Err(e))) => return Err(e),
                    Some(Err(e)) => return Err(e.to_string()),
                    Some(Ok(Ok(()))) | None => return Ok(()),
                }
            }
            poll = rest.get_updates(offset) => {
                match poll {
                    Ok(updates) => {
                        failures = 0;
                        for update in &updates {
                            let id = update.get("update_id").and_then(Value::as_i64).unwrap_or(0);
                            if let Some(cq) = update.get("callback_query") {
                                crate::graycmd::callback(
                                    &config, &store, &rest, cq, &owner_id, &allowed,
                                )
                                .await;
                            } else if let Some(m) = inbound(update, &bot_username, &bot_id) {
                                handle_inbound(
                                    &m,
                                    &config,
                                    config_path,
                                    &store,
                                    &rest,
                                    &limiter,
                                    &owner_id,
                                    &allowed,
                                    &bot_username,
                                    &workdir,
                                    max_attachment_bytes,
                                    &registry,
                                )
                                .await;
                            }
                            if id >= offset {
                                offset = id + 1;
                            }
                        }
                        let _ = store.meta_set(OFFSET_KEY, &offset.to_string());
                    }
                    Err(TransportError::RateLimited(after)) => {
                        tokio::time::sleep(std::time::Duration::from_secs_f64(
                            after.unwrap_or(5.0).clamp(1.0, 120.0),
                        ))
                        .await;
                    }
                    Err(error @ (TransportError::Auth(_) | TransportError::Forbidden(_))) => {
                        set.abort_all();
                        return Err(error.to_string());
                    }
                    Err(error) => {
                        eprintln!("[telegram] getUpdates failed: {error}");
                        tokio::time::sleep(poll_backoff(failures)).await;
                        failures = failures.saturating_add(1);
                    }
                }
            }
        }
    }

    // Clean exit: tell running chats their turn was cut, mark the state.
    if crate::lifecycle::enabled(&config) {
        let bye = crate::lifecycle::stop_via(&gray_bin, &dir).await;
        for channel in store.running_channels().unwrap_or_default() {
            if channel == home_channel {
                continue;
            }
            let _ = rest
                .send_text(&channel, &crate::text::escape_html(bye.active.as_str()))
                .await;
        }
        let _ = rest
            .send_text(&home_channel, &crate::text::escape_html(&bye.home))
            .await;
    }
    Ok(())
}

/// Admit and enqueue (or pair/deflect) one inbound message.
#[allow(clippy::too_many_arguments)]
async fn handle_inbound(
    m: &Inbound,
    config: &Value,
    config_path: &Path,
    store: &Store,
    rest: &Rest,
    limiter: &std::sync::Mutex<crate::ratelimit::RateLimiter>,
    owner_id: &str,
    allowed: &[String],
    bot_username: &str,
    workdir: &str,
    max_attachment_bytes: u64,
    registry: &crate::graycmd::Registry,
) {
    // Receipt log: every message the gateway sees, by surface. Never the
    // content — this is the only way to tell "never messaged" from "update
    // never arrived".
    eprintln!(
        "[telegram] update {} from {} in chat {} (private: {})",
        m.update_id, m.user_id, m.chat_id, m.is_private
    );

    let conversation = crate::session::conversation_key(&m.chat_id, &m.user_id, m.is_private);

    // Commands first: /start is the pairing hello for strangers and a
    // greeting for the admitted; the rest need admission. The registry
    // snapshot resolves `gray cmd` names — strangers stay on today's
    // fallback (unknown `/foo` falls through to the pairing path) and we
    // never spawn `cmd list` for them.
    let admitted = crate::policy::is_allowed_user(&m.user_id, owner_id, allowed);
    // The registry only matters for `/`-text; a plain prompt never spawns.
    let snapshot = if admitted && m.text.trim_start().starts_with('/') {
        registry.get().await
    } else {
        None
    };
    if let Some(cmd) = crate::commands::parse(&m.text, bot_username, snapshot.as_deref()) {
        use crate::commands::BotCommand::*;
        match cmd {
            Start => {
                if !admitted && m.is_private {
                    send_pairing(store, rest, m).await;
                } else {
                    let _ = rest
                        .send_text(
                            &m.chat_id,
                            "Connected to gray. Send a message and I will run it.",
                        )
                        .await;
                }
            }
            New => {
                if !admitted {
                    return send_if_stranger(store, rest, m).await;
                }
                let _ = store.cancel_pending_conversation(&conversation);
                if let Some(home) = runtime_home(config_path, &conversation) {
                    let _ = crate::session::reset_home(&home, crate::durable::now_secs());
                }
                let _ = rest
                    .send_text(&m.chat_id, "Fresh conversation started.")
                    .await;
            }
            Stop => {
                if !admitted {
                    return send_if_stranger(store, rest, m).await;
                }
                let stopped = store.cancel_conversation(&conversation).unwrap_or(false);
                let text = if stopped {
                    "Stopped the running task."
                } else {
                    "Nothing running."
                };
                let _ = rest.send_text(&m.chat_id, text).await;
            }
            Steer { ref text } => {
                if !admitted {
                    return send_if_stranger(store, rest, m).await;
                }
                let reply = if text.trim().is_empty() {
                    "Usage: /steer &lt;text&gt;".to_string()
                } else if crate::runner::steer(&conversation, text).await {
                    "Steering the current turn.".to_string()
                } else {
                    let capacity = config
                        .get("queue_capacity")
                        .and_then(Value::as_u64)
                        .unwrap_or(1000);
                    match store.enqueue(
                        &format!("tg:{}", m.update_id),
                        &m.chat_id,
                        text,
                        Some(&conversation),
                        capacity,
                    ) {
                        Ok(_) => "Nothing running — sent as a new message.".to_string(),
                        Err(_) => "Queue full or message invalid; this message was not accepted."
                            .to_string(),
                    }
                };
                let _ = rest.send_text(&m.chat_id, &reply).await;
            }
            Status => {
                if !admitted {
                    return send_if_stranger(store, rest, m).await;
                }
                let pending = store.pending_count().unwrap_or(0);
                let running = store.running_channels().unwrap_or_default().len();
                let _ = rest
                    .send_text(
                        &m.chat_id,
                        &crate::text::escape_html(&format!(
                            "Queue: {pending} pending, {running} chat(s) running."
                        )),
                    )
                    .await;
            }
            Help => {
                let text = crate::graycmd::help_text(snapshot.as_deref());
                let home =
                    runtime_home(config_path, &conversation).unwrap_or_else(|| PathBuf::from("."));
                crate::graycmd::send_paged(rest, store, &m.chat_id, &m.user_id, &home, &text).await;
            }
            Gray { ref name, ref args } => {
                if !admitted {
                    return send_if_stranger(store, rest, m).await;
                }
                crate::graycmd::dispatch(
                    registry,
                    config,
                    config_path,
                    store,
                    rest,
                    &m.chat_id,
                    &m.user_id,
                    &conversation,
                    name,
                    args,
                )
                .await;
            }
        }
        return;
    }
    // A command addressed to another bot in a group is not ours.
    if m.text.trim_start().starts_with('/') && !m.is_private && !m.mentioned {
        return;
    }

    let Some(mut prompt) = crate::policy::incoming(
        &m.user_id,
        owner_id,
        m.is_bot,
        m.is_private,
        &m.text,
        bot_username,
        m.mentioned,
        m.reply_to_bot,
        allowed,
    ) else {
        send_if_stranger(store, rest, m).await;
        return;
    };

    // Files the user sent: downloaded, then named in the prompt so gray's
    // own tools can read them.
    if !m.files.is_empty() {
        let mut saved: Vec<PathBuf> = Vec::new();
        for (file_id, name) in &m.files {
            let path = crate::attachments::save(
                rest,
                file_id,
                name,
                &format!("{}", m.update_id),
                Path::new(workdir),
                max_attachment_bytes,
            )
            .await;
            if let Some(p) = path {
                saved.push(p);
            }
        }
        if !saved.is_empty() {
            prompt = if prompt.trim().is_empty() {
                crate::attachments::prompt_lines(&saved)
            } else {
                format!("{}\n\n{}", prompt, crate::attachments::prompt_lines(&saved))
            };
        }
    }
    if prompt.trim().is_empty() {
        return;
    }

    // Burst guard: an over-eager user is told to slow down instead of
    // forking gray per message.
    let allowed_now = limiter
        .lock()
        .map(|mut l| l.allow(&m.user_id))
        .unwrap_or(true);
    if !allowed_now {
        let wait = limiter
            .lock()
            .map(|mut l| l.retry_after_secs(&m.user_id))
            .unwrap_or(1);
        let _ = rest
            .send_text(&m.chat_id, &format!("Too fast — try again in {wait}s."))
            .await;
        return;
    }

    let capacity = config
        .get("queue_capacity")
        .and_then(Value::as_u64)
        .unwrap_or(1000);
    if busy_input_steer(config) && crate::runner::steer(&conversation, &prompt).await {
        let _ = rest
            .send_text(&m.chat_id, "Steering the current turn.")
            .await;
        return;
    }
    let id = format!("tg:{}", m.update_id);
    match store.enqueue(&id, &m.chat_id, &prompt, Some(&conversation), capacity) {
        Ok(true) => {
            if let Some(notice) = queued_notice(store, &conversation, &id) {
                let _ = rest.send_text(&m.chat_id, &notice).await;
            }
        }
        Ok(false) => {}
        Err(_) => {
            let _ = rest
                .send_text(
                    &m.chat_id,
                    "Queue full or message invalid; this message was not accepted.",
                )
                .await;
        }
    }
}

fn runtime_home(config_path: &Path, conversation: &str) -> Option<PathBuf> {
    // The same layout `run_gray` builds: conversations live beside the
    // config file, keyed by sha256 of the conversation key.
    let base = config_path.parent()?;
    Some(
        base.join("conversations")
            .join(crate::runner::hex_sha256(conversation.as_bytes())),
    )
}

/// An unadmitted human in a private chat gets the pairing reply; everyone
/// else (bots, group strangers) is dropped silently.
async fn send_if_stranger(store: &Store, rest: &Rest, m: &Inbound) {
    if m.is_private && !m.is_bot {
        send_pairing(store, rest, m).await;
    }
}

async fn send_pairing(store: &Store, rest: &Rest, m: &Inbound) {
    if let Some(paired) = crate::pairing::reply_for(store, &m.user_id, m.is_private, m.is_bot) {
        match rest.send_text(&m.chat_id, &paired.text).await {
            Ok(_) => eprintln!("[telegram] pairing reply sent"),
            Err(e) => eprintln!("[telegram] pairing reply failed: {e}"),
        }
    }
}
