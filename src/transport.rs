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
            Self::Net(_) => f.write_str("Telegram request failed; check connectivity"),
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
#[derive(Debug, Clone)]
pub struct Rest {
    base: String,
    file_base: String,
    client: reqwest::Client,
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
        use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("gray-telegram/0.1"));
        // Every request is bounded. Without this a pooled connection that
        // died under us (a laptop's wifi, a NAT timeout) leaves a request
        // waiting forever, and the turn's messages freeze with it. Keepalive
        // lets a half-open long-poll error out instead of hanging (#87057).
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        let base = base.trim_end_matches('/').to_string();
        Self {
            file_base: format!("{base}/file/bot{token}"),
            base: format!("{base}/bot{token}"),
            client,
        }
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
        let resp = self
            .client
            .post(format!("{}/{method}", self.base))
            .json(params)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    TransportError::Net("request timed out".into())
                } else {
                    TransportError::Net("request failed".into())
                }
            })?;
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
        let req = self
            .client
            .post(format!("{}/getUpdates", self.base))
            .json(&json!({
                "offset": offset,
                "timeout": LONG_POLL_SECS,
                // The bridge reads message updates (text, photos, docs,
                // captions) and the `gc|` button presses inline keyboards
                // produce. Skipping the rest keeps the wire small.
                "allowed_updates": ["message", "callback_query"]
            }))
            .timeout(POLL_TIMEOUT);
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                TransportError::Net("long poll timed out".into())
            } else {
                TransportError::Net("long poll failed".into())
            }
        })?;
        let result = self.classify(resp).await?;
        Ok(result.as_array().map(|a| a.to_vec()).unwrap_or_default())
    }

    /// `sendMessage` with HTML parse mode. A body Telegram cannot parse as
    /// HTML (a stray `<` in model output) retries once as plain text rather
    /// than failing the delivery.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<Value, TransportError> {
        self.send_message(chat_id, text, Value::Null).await
    }

    /// `sendMessage` carrying an inline keyboard (`reply_markup`). The same
    /// HTML→plain retry applies: a refused body never strands the buttons.
    pub async fn send_markup(
        &self,
        chat_id: &str,
        text: &str,
        markup: &Value,
    ) -> Result<Value, TransportError> {
        self.send_message(chat_id, text, markup.clone()).await
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        markup: Value,
    ) -> Result<Value, TransportError> {
        check_bounds(
            text,
            MAX_MESSAGE_LEN,
            "Message must contain 1-4096 characters",
        )?;
        let mut params = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if markup.is_object() {
            params["reply_markup"] = markup;
        }
        match self.call("sendMessage", &params).await {
            Err(TransportError::Http(400, detail))
                if detail.to_ascii_lowercase().contains("parse") =>
            {
                params.as_object_mut().map(|p| p.remove("parse_mode"));
                self.call("sendMessage", &params).await
            }
            other => other,
        }
    }

    /// `editMessageText` on a picker message. `markup` replaces the
    /// keyboard (`{"inline_keyboard":[]}` drops it); `None` leaves it.
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
            Err(TransportError::Http(400, detail))
                if detail.to_ascii_lowercase().contains("parse") =>
            {
                params.as_object_mut().map(|p| p.remove("parse_mode"));
                self.call("editMessageText", &params).await
            }
            other => other,
        }
    }

    /// `answerCallbackQuery` — dismisses the client's loading spinner; the
    /// optional `text` pops as a toast (`alert` for errors it must see).
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
        let resp = self
            .client
            .get(format!("{}/{}", self.file_base, file_path))
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|_| TransportError::Net("download failed".into()))?;
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

    /// `sendPhoto` — one image upload with an optional HTML caption.
    pub async fn send_photo(
        &self,
        chat_id: &str,
        path: &Path,
        caption: Option<&str>,
    ) -> Result<Value, TransportError> {
        self.send_upload("sendPhoto", "photo", chat_id, path, caption)
            .await
    }

    /// `sendDocument` — any file upload with an optional HTML caption.
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
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name);
        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part(field.to_string(), part);
        if let Some(caption) = caption {
            if !caption.trim().is_empty() && caption.chars().count() <= MAX_CAPTION_LEN {
                form = form
                    .text("caption", caption.to_string())
                    .text("parse_mode", "HTML".to_string());
            }
        }
        let resp = self
            .client
            .post(format!("{}/{method}", self.base))
            .multipart(form)
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|_| TransportError::Net("upload failed".into()))?;
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

/// Bounds-check text before it leaves the process; counts characters like
/// Telegram (code points), so emoji-heavy messages are not wrongly refused.
fn check_bounds(text: &str, max: usize, msg: &str) -> Result<(), TransportError> {
    if text.trim().is_empty() || text.chars().count() > max {
        return Err(TransportError::Invalid(msg.to_string()));
    }
    Ok(())
}
