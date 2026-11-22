//! Telegram Bot API transport: long-poll `getUpdates` plus the REST calls
//! the bridge needs. Auth is the token in the URL path (`/bot<token>/`), so
//! no header ever carries it; nothing here logs a token.
//!
//! Semantics lifted from the Hermes adapter (`plugins/platforms/telegram`):
//! offset-tracked long polling with `allowed_updates` narrowed to what the
//! bridge reads, `retry_after` honored on flood control, every request
//! bounded so a wedged socket cannot freeze the loop, and a graceful
//! reconnect-plus-backoff driven by the caller (it owns the offset).
use reqwest::multipart::Form;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// Production Bot API base. Tests inject a loopback stub via [`Rest::new`];
/// production call sites always use [`Rest::production`] (never read a base
/// URL from user config).
pub const API_BASE: &str = "https://api.telegram.org";
/// sendMessage text limit (chars, per Bot API docs).
pub const MAX_MESSAGE_LEN: usize = 4096;
/// Media caption limit (chars).
pub const MAX_CAPTION_LEN: usize = 1024;
/// getUpdates long-poll: Telegram holds the request this long.
pub const LONG_POLL_SECS: u64 = 30;
/// getFile download cap: the public Bot API refuses files over 20 MiB.
pub const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum TransportError {
    Auth(String),
    Forbidden(String),
    RateLimited(Option<f64>),
    Http(u16, String),
    Net(String),
    Invalid(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(_) => f.write_str("Telegram authentication failed; check the bot token"),
            Self::Forbidden(_) => f.write_str(
                "Telegram refused the request; the bot may be blocked or removed from the chat",
            ),
            Self::RateLimited(_) => f.write_str("Telegram rate limit hit; backing off"),
            Self::Http(status @ (400 | 413), detail) => {
                write!(f, "Telegram rejected the request (HTTP {status}): {detail}")
            }
            Self::Http(status, _) => write!(f, "Telegram HTTP {status}"),
            Self::Net(detail) => write!(f, "Telegram request failed: {detail}"),
            Self::Invalid(msg) => f.write_str(msg),
        }
    }
}
impl std::error::Error for TransportError {}

impl TransportError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited(_) | Self::Net(_) => true,
            Self::Http(s, _) => *s == 429 || *s >= 500,
            _ => false,
        }
    }
}

/// A request that has not answered in this long is given up as a network
/// error (retried by its caller), never waited on forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Long-poll reads outlast the normal budget by the poll itself plus slack.
const POLL_TIMEOUT: Duration = Duration::from_secs(LONG_POLL_SECS + 15);
/// Uploads carry files and get longer.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Thin Bot API client. `base` is a constructor arg so tests can inject a
/// loopback stub; production call sites always pass `API_BASE`.
const SEED_FALLBACK_IPS: [&str; 2] = ["149.154.166.110", "149.154.167.220"];

#[derive(Debug, Clone)]
pub struct Rest {
    base: String,
    file_base: String,
    clients: Vec<reqwest::Client>,
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    rich_supported: std::sync::Arc<std::sync::atomic::AtomicBool>,
    flood_until:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
}

fn resolve_proxy_url() -> Option<String> {
    for key in [
        "TELEGRAM_PROXY",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn build_client(
    resolve_ip: Option<std::net::Ipv4Addr>,
    proxy_url: Option<&str>,
) -> reqwest::Client {
    use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("gray-telegram/0.1"));
    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(30));
    if let Some(proxy) = proxy_url.and_then(|u| reqwest::Proxy::all(u).ok()) {
        builder = builder.proxy(proxy);
    } else if let Some(ip) = resolve_ip {
        builder = builder.resolve(
            "api.telegram.org",
            std::net::SocketAddr::new(std::net::IpAddr::V4(ip), 443),
        );
    }
    builder.build().expect("reqwest client builds")
}

/// Telegram's error body describes our request, never credentials:
/// `{error_code, description, parameters{retry_after}}`.
fn error_detail(body: &Value) -> String {
    body.get("description")
        .and_then(Value::as_str)
        .unwrap_or("request failed")
        .to_string()
}

fn retry_after(body: &Value) -> Option<f64> {
    body.pointer("/parameters/retry_after")
        .and_then(Value::as_f64)
}

impl Rest {
    pub fn new(base: &str, token: &str) -> Self {
        let base = base.trim_end_matches('/').to_string();
        let proxy = resolve_proxy_url();
        let mut clients = vec![build_client(None, proxy.as_deref())];
        if base == API_BASE && proxy.is_none() {
            for raw in SEED_FALLBACK_IPS {
                if let Ok(ip) = raw.parse::<std::net::Ipv4Addr>() {
                    clients.push(build_client(Some(ip), None));
                }
            }
        }
        Self {
            file_base: format!("{base}/file/bot{token}"),
            base: format!("{base}/bot{token}"),
            clients,
            active: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            rich_supported: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            flood_until: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    pub async fn with_doh_ips(mut self) -> Self {
        if !self.base.starts_with(API_BASE) || resolve_proxy_url().is_some() {
            return self;
        }
        let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
        else {
            return self;
        };
        if let Ok(resp) = client
            .get("https://dns.google/resolve?name=api.telegram.org&type=A")
            .send()
            .await
        {
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(ans) = body.get("Answer").and_then(Value::as_array) {
                    for row in ans {
                        if let Some(ip_str) = row.get("data").and_then(Value::as_str) {
                            if !SEED_FALLBACK_IPS.contains(&ip_str) {
                                if let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() {
                                    self.clients.push(build_client(Some(ip), None));
                                }
                            }
                        }
                    }
                }
            }
        }
        self
    }

    pub fn rich_supported(&self) -> bool {
        self.rich_supported
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn flood_remaining(&self, chat_id: &str) -> Option<Duration> {
        let mut map = self.flood_until.lock().ok()?;
        let now = std::time::Instant::now();
        map.retain(|_, until| *until > now);
        map.get(chat_id)
            .map(|until| until.saturating_duration_since(now))
    }

    fn record_flood(&self, chat_id: &str, secs: f64) {
        if let Ok(mut map) = self.flood_until.lock() {
            let until = std::time::Instant::now() + Duration::from_secs_f64(secs.clamp(1.0, 60.0));
            map.insert(chat_id.to_string(), until);
        }
    }

    async fn call_thread_safe(
        &self,
        method: &str,
        params: &mut Value,
    ) -> Result<Value, TransportError> {
        match self.call(method, params).await {
            Err(TransportError::Http(400, ref detail))
                if params.get("message_thread_id").is_some()
                    && (detail.to_ascii_lowercase().contains("thread not found")
                        || detail.contains("TOPIC_DELETED")
                        || detail.contains("TOPIC_CLOSED")) =>
            {
                if let Some(obj) = params.as_object_mut() {
                    obj.remove("message_thread_id");
                }
                self.call(method, params).await
            }
            other => other,
        }
    }

    async fn send_with_fallback<F>(&self, mut build: F) -> Result<reqwest::Response, TransportError>
    where
        F: FnMut(&reqwest::Client) -> reqwest::RequestBuilder,
    {
        use std::sync::atomic::Ordering;
        let len = self.clients.len();
        let start = self.active.load(Ordering::Relaxed) % len;
        let mut last_err = String::new();
        for offset in 0..len {
            let idx = (start + offset) % len;
            match build(&self.clients[idx]).send().await {
                Ok(resp) => {
                    if idx != start {
                        self.active.store(idx, Ordering::Relaxed);
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    last_err = e.without_url().to_string();
                }
            }
        }
        Err(TransportError::Net(last_err))
    }

    pub fn production(token: &str) -> Self {
        // Test builds may retarget the "production" client at a loopback
        // Bot API stub via GRAY_TELEGRAM_TEST_API_BASE. The branch is
        // compiled out of release and debug binaries, so a config value
        // or stray env var can never redirect the token-bearing requests.
        #[cfg(test)]
        if let Ok(base) = std::env::var("GRAY_TELEGRAM_TEST_API_BASE") {
            let base = base.trim().to_string();
            if !base.is_empty() {
                return Self::new(&base, token);
            }
        }
        Self::new(API_BASE, token)
    }

    async fn classify(&self, resp: reqwest::Response) -> Result<Value, TransportError> {
        let code = resp.status().as_u16();
        let body = resp.json::<Value>().await.unwrap_or(Value::Null);
        if code == 401 {
            return Err(TransportError::Auth("unauthorized".into()));
        }
        if code == 403 {
            return Err(TransportError::Forbidden(error_detail(&body)));
        }
        if code == 429 {
            return Err(TransportError::RateLimited(retry_after(&body)));
        }
        // Bot API answers 200+{ok:true,result} or 4xx/5xx+{ok:false}; a 200
        // with ok:false still lands here as a failed call.
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            if code == 400 || code == 413 {
                return Err(TransportError::Http(code, error_detail(&body)));
            }
            return Err(TransportError::Http(code.max(400), error_detail(&body)));
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// POST a JSON-RPC-shaped Bot API method.
    async fn call(&self, method: &str, params: &Value) -> Result<Value, TransportError> {
        let url = format!("{}/{method}", self.base);
        let resp = self
            .send_with_fallback(|c| c.post(&url).json(params))
            .await?;
        self.classify(resp).await
    }

    /// `getMe` — token check and the bot's own id/username.
    pub async fn get_me(&self) -> Result<Value, TransportError> {
        self.call("getMe", &json!({})).await
    }

    /// `getUpdates` long-poll. `offset` = highest confirmed update_id + 1;
    /// Telegram drops every update below it, so persisting the offset after
    /// enqueue is exactly-once-enough (redelivery only replays unacked ids).
    pub async fn get_updates(&self, offset: i64) -> Result<Vec<Value>, TransportError> {
        let url = format!("{}/getUpdates", self.base);
        let body = json!({
            "offset": offset,
            "timeout": LONG_POLL_SECS,
            "allowed_updates": ["message", "callback_query", "inline_query"]
        });
        let resp = self
            .send_with_fallback(|c| c.post(&url).json(&body).timeout(POLL_TIMEOUT))
            .await?;
        let result = self.classify(resp).await?;
        Ok(result.as_array().map(|a| a.to_vec()).unwrap_or_default())
    }

    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<Value, TransportError> {
        self.send_message_in(chat_id, None, text, Value::Null).await
    }

    pub async fn send_text_in(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
        text: &str,
    ) -> Result<Value, TransportError> {
        self.send_message_in(chat_id, thread_id, text, Value::Null)
            .await
    }

    pub async fn send_markup(
        &self,
        chat_id: &str,
        text: &str,
        markup: &Value,
    ) -> Result<Value, TransportError> {
        self.send_message_in(chat_id, None, text, markup.clone())
            .await
    }

    pub async fn send_markup_in(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
        text: &str,
        markup: &Value,
    ) -> Result<Value, TransportError> {
        self.send_message_in(chat_id, thread_id, text, markup.clone())
            .await
    }

    async fn send_message_in(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
        text: &str,
        markup: Value,
    ) -> Result<Value, TransportError> {
        check_bounds(
            text,
            MAX_MESSAGE_LEN,
            "Message must contain 1-4096 characters",
        )?;
        if let Some(wait) = self.flood_remaining(chat_id) {
            tokio::time::sleep(wait).await;
        }
        let mut params = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(tid) = thread_id {
            params["message_thread_id"] = json!(tid);
        }
        if markup.is_object() {
            params["reply_markup"] = markup;
        }
        let res = match self.call_thread_safe("sendMessage", &mut params).await {
            Err(TransportError::Http(400, detail)) if html_refused(&detail) => {
                let plain = crate::render::html_to_text(text);
                params["text"] = Value::String(plain);
                if let Some(fields) = params.as_object_mut() {
                    fields.remove("parse_mode");
                }
                self.call_thread_safe("sendMessage", &mut params).await
            }
            other => other,
        };
        if let Err(TransportError::RateLimited(Some(secs))) = &res {
            self.record_flood(chat_id, *secs);
        }
        res
    }

    pub async fn send_rich_message_in(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
        markdown: &str,
    ) -> Result<Value, TransportError> {
        if !self.rich_supported() {
            return Err(TransportError::Http(400, "rich unsupported".into()));
        }
        if let Some(wait) = self.flood_remaining(chat_id) {
            tokio::time::sleep(wait).await;
        }
        let mut params = json!({
            "chat_id": chat_id,
            "rich_message": {"markdown": markdown},
        });
        if markdown.contains('@') {
            params["disable_entity_detection"] = json!(true);
        }
        if let Some(tid) = thread_id {
            params["message_thread_id"] = json!(tid);
        }
        let res = self.call_thread_safe("sendRichMessage", &mut params).await;
        match &res {
            Err(TransportError::Http(400 | 404, detail)) => {
                let lower = detail.to_ascii_lowercase();
                if lower.contains("not found")
                    || lower.contains("unknown")
                    || lower.contains("unsupported")
                    || lower.contains("rich")
                {
                    self.rich_supported
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Err(TransportError::RateLimited(Some(secs))) => {
                self.record_flood(chat_id, *secs);
            }
            _ => {}
        }
        res
    }

    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: i64,
        text: &str,
        markup: Option<&Value>,
    ) -> Result<Value, TransportError> {
        check_bounds(
            text,
            MAX_MESSAGE_LEN,
            "Message must contain 1-4096 characters",
        )?;
        let mut params = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "parse_mode": "HTML",
        });
        if let Some(markup) = markup {
            params["reply_markup"] = markup.clone();
        }
        match self.call("editMessageText", &params).await {
            Err(TransportError::Http(400, detail)) if html_refused(&detail) => {
                let plain = crate::render::html_to_text(text);
                params["text"] = Value::String(plain);
                if let Some(fields) = params.as_object_mut() {
                    fields.remove("parse_mode");
                }
                self.call("editMessageText", &params).await
            }
            other => other,
        }
    }

    pub async fn delete_message(
        &self,
        chat_id: &str,
        message_id: i64,
    ) -> Result<(), TransportError> {
        self.call(
            "deleteMessage",
            &json!({"chat_id": chat_id, "message_id": message_id}),
        )
        .await
        .map(|_| ())
    }

    pub async fn set_reaction(
        &self,
        chat_id: &str,
        message_id: i64,
        emoji: Option<&str>,
    ) -> Result<(), TransportError> {
        let reaction = match emoji {
            Some(emoji) => json!([{"type": "emoji", "emoji": emoji}]),
            None => json!([]),
        };
        self.call(
            "setMessageReaction",
            &json!({"chat_id": chat_id, "message_id": message_id, "reaction": reaction}),
        )
        .await
        .map(|_| ())
    }

    pub async fn send_draft(
        &self,
        chat_id: &str,
        draft_id: i64,
        text: &str,
    ) -> Result<(), TransportError> {
        self.send_draft_in(chat_id, None, draft_id, text).await
    }

    pub async fn send_draft_in(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
        draft_id: i64,
        text: &str,
    ) -> Result<(), TransportError> {
        check_bounds(
            text,
            MAX_MESSAGE_LEN,
            "Draft must contain 1-4096 characters",
        )?;
        if self.flood_remaining(chat_id).is_some() {
            return Ok(());
        }
        let mut params = json!({
            "chat_id": chat_id,
            "draft_id": draft_id,
            "text": text,
            "parse_mode": "HTML",
        });
        if let Some(tid) = thread_id {
            params["message_thread_id"] = json!(tid);
        }
        let res = match self.call_thread_safe("sendMessageDraft", &mut params).await {
            Err(TransportError::Http(400, detail)) if html_refused(&detail) => {
                params["text"] = Value::String(crate::render::html_to_text(text));
                if let Some(fields) = params.as_object_mut() {
                    fields.remove("parse_mode");
                }
                self.call_thread_safe("sendMessageDraft", &mut params)
                    .await
                    .map(|_| ())
            }
            other => other.map(|_| ()),
        };
        if let Err(TransportError::RateLimited(Some(secs))) = &res {
            self.record_flood(chat_id, *secs);
        }
        res
    }

    pub async fn answer_callback(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
        alert: bool,
    ) -> Result<(), TransportError> {
        self.call(
            "answerCallbackQuery",
            &json!({
                "callback_query_id": callback_query_id,
                "text": text.unwrap_or(""),
                "show_alert": alert,
            }),
        )
        .await
        .map(|_| ())
    }

    /// `setMyCommands` — the visible slash menu Telegram clients show.
    pub async fn set_my_commands(&self, commands: &[Value]) -> Result<(), TransportError> {
        self.call("setMyCommands", &json!({"commands": commands}))
            .await
            .map(|_| ())
    }

    pub async fn set_my_commands_scoped(
        &self,
        commands: &[Value],
        scope: Value,
    ) -> Result<(), TransportError> {
        self.call(
            "setMyCommands",
            &json!({"commands": commands, "scope": scope}),
        )
        .await
        .map(|_| ())
    }

    pub async fn answer_inline_query(
        &self,
        inline_query_id: &str,
        results: &[Value],
    ) -> Result<(), TransportError> {
        self.call(
            "answerInlineQuery",
            &json!({
                "inline_query_id": inline_query_id,
                "results": results,
                "cache_time": 10,
            }),
        )
        .await
        .map(|_| ())
    }

    pub async fn send_typing_in(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
    ) -> Result<(), TransportError> {
        let mut params = json!({"chat_id": chat_id, "action": "typing"});
        if let Some(tid) = thread_id {
            params["message_thread_id"] = json!(tid);
        }
        self.call_thread_safe("sendChatAction", &mut params)
            .await
            .map(|_| ())
    }

    pub async fn send_media_group(
        &self,
        chat_id: &str,
        thread_id: Option<i64>,
        kind: &str,
        paths: &[&Path],
        caption: Option<&str>,
    ) -> Result<Value, TransportError> {
        let url = format!("{}/sendMediaGroup", self.base);
        let mut media = Vec::with_capacity(paths.len());
        let mut files: Vec<(String, String, Vec<u8>)> = Vec::with_capacity(paths.len());
        for (idx, path) in paths.iter().take(10).enumerate() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string();
            let bytes = std::fs::read(path)
                .map_err(|_| TransportError::Invalid("cannot read file".into()))?;
            let attach = format!("file{idx}");
            let mut item = json!({"type": kind, "media": format!("attach://{attach}")});
            if idx == 0 {
                if let Some(cap) = caption.filter(|c| !c.trim().is_empty()) {
                    let clipped: String = cap.chars().take(MAX_CAPTION_LEN).collect();
                    item["caption"] = json!(clipped);
                    item["parse_mode"] = json!("HTML");
                }
            }
            media.push(item);
            files.push((attach, name, bytes));
        }
        let media_json = serde_json::to_string(&media).unwrap_or_else(|_| "[]".to_string());
        let resp = self
            .send_with_fallback(|c| {
                let mut form = Form::new()
                    .text("chat_id", chat_id.to_string())
                    .text("media", media_json.clone());
                if let Some(tid) = thread_id {
                    form = form.text("message_thread_id", tid.to_string());
                }
                for (attach, name, bytes) in &files {
                    let part =
                        reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone());
                    form = form.part(attach.clone(), part);
                }
                c.post(&url).multipart(form).timeout(UPLOAD_TIMEOUT)
            })
            .await?;
        self.classify(resp).await
    }

    /// `sendChatAction` — the typing bubble. Best-effort by the caller.
    pub async fn typing(&self, chat_id: &str) -> Result<(), TransportError> {
        self.call(
            "sendChatAction",
            &json!({"chat_id": chat_id, "action": "typing"}),
        )
        .await
        .map(|_| ())
    }

    /// `getFile` → the temporary `file_path` used by [`Self::download`].
    pub async fn get_file(&self, file_id: &str) -> Result<(String, u64), TransportError> {
        if file_id.trim().is_empty() || file_id.len() > 256 {
            return Err(TransportError::Invalid("invalid file_id".into()));
        }
        let result = self.call("getFile", &json!({"file_id": file_id})).await?;
        let path = result
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| TransportError::Invalid("getFile returned no path".into()))?;
        let size = result.get("file_size").and_then(Value::as_u64).unwrap_or(0);
        Ok((path.to_string(), size))
    }

    /// Download a `file_path` from the Bot API file endpoint, bounded.
    pub async fn download(
        &self,
        file_path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, TransportError> {
        if file_path.contains("..") || file_path.starts_with('/') {
            return Err(TransportError::Invalid("invalid file path".into()));
        }
        let url = format!("{}/{}", self.file_base, file_path);
        let resp = self
            .send_with_fallback(|c| c.get(&url).timeout(UPLOAD_TIMEOUT))
            .await?;
        if !resp.status().is_success() {
            return Err(TransportError::Http(
                resp.status().as_u16(),
                "download failed".into(),
            ));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|_| TransportError::Net("download failed".into()))?;
        if bytes.len() as u64 > max_bytes {
            return Err(TransportError::Invalid("file too large".into()));
        }
        Ok(bytes.to_vec())
    }

    pub async fn send_photo(
        &self,
        chat_id: &str,
        path: &Path,
        caption: Option<&str>,
    ) -> Result<Value, TransportError> {
        self.send_upload("sendPhoto", "photo", chat_id, path, caption)
            .await
    }

    pub async fn send_document(
        &self,
        chat_id: &str,
        path: &Path,
        caption: Option<&str>,
    ) -> Result<Value, TransportError> {
        self.send_upload("sendDocument", "document", chat_id, path, caption)
            .await
    }

    async fn send_upload(
        &self,
        method: &str,
        field: &str,
        chat_id: &str,
        path: &Path,
        caption: Option<&str>,
    ) -> Result<Value, TransportError> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let bytes =
            std::fs::read(path).map_err(|_| TransportError::Invalid("cannot read file".into()))?;
        match self
            .post_upload(method, field, chat_id, &name, &bytes, caption, true)
            .await
        {
            Err(TransportError::Http(400, detail))
                if caption.is_some() && html_refused(&detail) =>
            {
                let plain = caption.map(crate::render::html_to_text);
                self.post_upload(
                    method,
                    field,
                    chat_id,
                    &name,
                    &bytes,
                    plain.as_deref(),
                    false,
                )
                .await
            }
            other => other,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn post_upload(
        &self,
        method: &str,
        field: &str,
        chat_id: &str,
        name: &str,
        bytes: &[u8],
        caption: Option<&str>,
        parse: bool,
    ) -> Result<Value, TransportError> {
        let url = format!("{}/{method}", self.base);
        let resp = self
            .send_with_fallback(|c| {
                let part =
                    reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(name.to_string());
                let mut form = Form::new()
                    .text("chat_id", chat_id.to_string())
                    .part(field.to_string(), part);
                if parse {
                    if let Some(caption) = caption.filter(|c| !c.trim().is_empty()) {
                        if caption.chars().count() <= MAX_CAPTION_LEN {
                            form = form
                                .text("caption", caption.to_string())
                                .text("parse_mode", "HTML".to_string());
                        }
                    }
                } else if let Some(caption) = caption.filter(|c| !c.trim().is_empty()) {
                    let clipped: String = caption.chars().take(MAX_CAPTION_LEN).collect();
                    form = form.text("caption", clipped);
                }
                c.post(&url).multipart(form).timeout(UPLOAD_TIMEOUT)
            })
            .await?;
        self.classify(resp).await
    }

    /// `getChat` — doctor's delivery-target check.
    pub async fn get_chat(&self, chat_id: &str) -> Result<Value, TransportError> {
        self.call("getChat", &json!({"chat_id": chat_id})).await
    }

    /// `deleteWebhook` — a stale webhook silently steals every update the
    /// long-poll should get; the gateway clears it at boot (and confirms the
    /// token in the same call's success).
    pub async fn delete_webhook(&self) -> Result<(), TransportError> {
        self.call("deleteWebhook", &json!({"drop_pending_updates": false}))
            .await
            .map(|_| ())
    }
}

fn html_refused(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "parse entities",
        "can't parse",
        "unmatched end tag",
        "unexpected end tag",
        "can't find end tag",
        "unsupported start tag",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

/// Bounds-check text before it leaves the process; counts characters like
/// Telegram (code points), so emoji-heavy messages are not wrongly refused.
fn check_bounds(text: &str, max: usize, msg: &str) -> Result<(), TransportError> {
    if text.trim().is_empty() || text.chars().count() > max {
        return Err(TransportError::Invalid(msg.to_string()));
    }
    Ok(())
}
