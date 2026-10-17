//! NDJSON protocol 1.1 sidecar.
//! Manifest answers without config; delivery failures never echo secrets.
use serde_json::{json, Value};
use std::path::Path;

use std::sync::LazyLock;

/// Static manifest (`telegram`, protocol 1.1, no commands).
pub static MANIFEST: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "name": "telegram",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "requires_plugins": ["gateway"],
        "commands": [],
        "hooks": ["prompt/context"],
        "tools": [{
            "name": "telegram_send",
            "label": "Telegram Send",
            "description": "Send text to the owner's Telegram chat. HTML markup is allowed (Telegram parse_mode=HTML: <b>, <i>, <code>, <pre>, <a href>, <tg-spoiler>). Long text is chunked at 4096 characters. Put MEDIA:/absolute/path.png on its own line to upload files with it (images arrive as photos, others as documents).",
            "parameters": {
                "type": "object",
                "properties": {"content": {"type": "string"}},
                "required": ["content"]
            }
        }, {
            "name": "telegram_file",
            "label": "Telegram File",
            "description": "Post local files to the owner's Telegram chat. Images (png/jpg/gif/webp) go through sendPhoto, everything else through sendDocument, with an optional HTML caption. Any readable path works except credentials/config files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (absolute, ~/..., or relative to the working directory)"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "up to 10 files, sent one message each"},
                    "caption": {"type": "string", "description": "optional caption (HTML, max 1024 chars)"}
                }
            }
        }]
    })
});

const PROMPT_CONTEXT: &str = "Telegram tools: telegram_send posts text with HTML markup (<b>, <i>, <code>, <pre>, <a href>, <tg-spoiler>); add a line MEDIA:/absolute/path to attach a file. telegram_file posts local files (images as photos, others as documents) with an optional caption. Replies are chunked at 4096 characters. Never send secrets.";

fn is_error() -> Value {
    json!({
        "content": "Telegram delivery failed. Run gray-telegram doctor; do not blindly retry partial sends.",
        "is_error": true
    })
}

/// A specific, fixable failure. `detail` must never contain config values.
fn not_sent(detail: impl std::fmt::Display) -> Value {
    json!({
        "content": format!("Not sent: {detail}. Nothing reached Telegram; fix it and call again."),
        "is_error": true
    })
}

/// Failure after Telegram was contacted; the detail is Telegram's own reason.
fn rejected(error: &crate::transport::TransportError) -> Value {
    json!({
        "content": format!("Not sent: {error}. Nothing was posted; fix the request and call again."),
        "is_error": true
    })
}

/// Dispatch one protocol method. Never echoes config/token text: failures
/// describe the caller's own input or Telegram's validation response only.
pub async fn dispatch(method: &str, params: &Value, config_path: &Path) -> Value {
    if method == "plugin/manifest" {
        return MANIFEST.clone();
    }
    if method == "prompt/context" {
        return json!({"text": PROMPT_CONTEXT});
    }
    if method == "tool/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        return match name {
            "telegram_send" => send_text(params, config_path).await,
            "telegram_file" => file_tool(params, config_path).await,
            _ => is_error(),
        };
    }
    json!({"error": "Unsupported method"})
}

/// The calling session's working directory, falling back to the sidecar's.
fn session_cwd(params: &Value) -> std::path::PathBuf {
    params
        .pointer("/session/cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|cwd| cwd.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(std::env::temp_dir)
}

/// Token and home chat from config; `None` means the bridge is not set up.
fn delivery_target(config_path: &Path) -> Result<(Value, String, String), Value> {
    let config = crate::config::load_config(config_path).map_err(|_| is_error())?;
    let token = crate::config::token(&config).ok_or_else(is_error)?;
    let chat = config
        .get("chat_id")
        .and_then(Value::as_str)
        .ok_or_else(is_error)?
        .to_string();
    Ok((config, token, chat))
}

async fn send_text(params: &Value, config_path: &Path) -> Value {
    let content = params
        .get("args")
        .and_then(|a| a.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if content.trim().is_empty() {
        return not_sent("content is empty");
    }
    let (config, token, chat) = match delivery_target(config_path) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let roots = crate::media_tags::roots_from_config(&config);
    let (prose, media) = crate::media_tags::extract(content, &session_cwd(params), &roots);
    let rest = crate::transport::Rest::production(&token);
    if !prose.trim().is_empty() {
        let chunks = match crate::text::split_message(
            &prose,
            crate::transport::MAX_MESSAGE_LEN - 48,
        ) {
            Ok(c) => c,
            Err(e) => return not_sent(e),
        };
        for chunk in chunks.iter().take(8) {
            let send = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                rest.send_text(&chat, chunk),
            )
            .await;
            match send {
                Ok(Ok(_)) => {}
                Ok(Err(error @ crate::transport::TransportError::Http(400, _))) => {
                    return rejected(&error)
                }
                _ => return is_error(),
            }
        }
        if chunks.len() > 8 {
            let _ = rest
                .send_text(&chat, "(answer truncated: too long for Telegram)")
                .await;
        }
    }
    if !media.is_empty() {
        if let Err(error) = upload(&rest, &chat, &media).await {
            return error;
        }
    }
    let files = if media.is_empty() {
        String::new()
    } else {
        format!(" with {} file(s)", media.len())
    };
    json!({"content": format!("Sent to the configured Telegram chat{files}.")})
}

/// Upload local files one message each (Telegram has no gallery upload for
/// documents); photos go through sendPhoto, the rest sendDocument.
async fn upload(
    rest: &crate::transport::Rest,
    chat: &str,
    paths: &[std::path::PathBuf],
) -> Result<usize, Value> {
    let uploads = crate::media_tags::load(paths);
    if uploads.is_empty() {
        return Err(not_sent("no readable files to upload"));
    }
    let mut sent = 0;
    for file in uploads.iter().take(10) {
        let send = if file.is_photo() {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                rest.send_photo(chat, &file.path, None),
            )
            .await
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                rest.send_document(chat, &file.path, None),
            )
            .await
        };
        match send {
            Ok(Ok(_)) => sent += 1,
            Ok(Err(error @ crate::transport::TransportError::Http(400 | 413, _))) => {
                return Err(rejected(&error))
            }
            _ if sent > 0 => {
                return Err(json!({
                    "content": format!("Partially sent: {sent} file(s) reached Telegram before a failure; do not resend those."),
                    "is_error": true
                }))
            }
            _ => return Err(is_error()),
        }
    }
    Ok(sent)
}

async fn file_tool(params: &Value, config_path: &Path) -> Value {
    let Some(args) = params.get("args") else {
        return not_sent("missing arguments");
    };
    let cwd = session_cwd(params);
    let mut raw_paths: Vec<String> = args
        .get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        raw_paths.insert(0, path.to_string());
    }
    if raw_paths.is_empty() {
        return not_sent("needs `path` or `paths`");
    }
    let (config, token, chat) = match delivery_target(config_path) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let roots = crate::media_tags::roots_from_config(&config);
    let mut paths = Vec::new();
    for raw in &raw_paths {
        match resolve_local(raw, &cwd, &roots) {
            Ok(path) => paths.push(path),
            Err(error) => return not_sent(error),
        }
    }
    let rest = crate::transport::Rest::production(&token);
    let caption = args.get("caption").and_then(Value::as_str);
    let mut sent = 0;
    for upload in crate::media_tags::load(&paths).into_iter().take(10) {
        let cap = if sent == 0 { caption } else { None };
        let send = if upload.is_photo() {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                rest.send_photo(&chat, &upload.path, cap),
            )
            .await
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                rest.send_document(&chat, &upload.path, cap),
            )
            .await
        };
        match send {
            Ok(Ok(_)) => sent += 1,
            Ok(Err(error @ crate::transport::TransportError::Http(400 | 413, _))) => {
                return rejected(&error)
            }
            _ if sent > 0 => {
                return json!({
                    "content": format!("Partially sent: {sent} file(s) reached Telegram before a failure; do not resend those."),
                    "is_error": true
                })
            }
            _ => return is_error(),
        }
    }
    if sent == 0 {
        return not_sent("no readable files to upload");
    }
    json!({"content": format!("Posted {sent} file(s) to the configured Telegram chat.")})
}

/// Resolve a caller path (absolute, `~/`, or relative to the session cwd)
/// and refuse credentials, config, oversized files, and anything outside
/// configured `media_roots`.
fn resolve_local(
    raw: &str,
    cwd: &Path,
    roots: &[std::path::PathBuf],
) -> Result<std::path::PathBuf, String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("MEDIA:").unwrap_or(raw).trim();
    let path = crate::media_tags::resolve(raw, cwd).ok_or_else(|| "path is empty".to_string())?;
    crate::media_tags::deliverable(&path, roots)
}

/// Blocking stdin loop. Exits 0 on EOF/`plugin/shutdown`, 1 on wire overflow.
pub fn serve(config_path: &Path) -> ! {
    use std::io::{BufRead, Write};
    const LIMIT: usize = 256 * 1024;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("sidecar runtime builds");
    loop {
        let mut buf: Vec<u8> = Vec::new();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => std::process::exit(1),
        };
        if n == 0 {
            std::process::exit(0);
        }
        if buf.len() > LIMIT {
            std::process::exit(1);
        }
        let text = match String::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !req.is_object() {
            continue;
        }
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            std::process::exit(0);
        }
        let id = match req.get("id") {
            Some(Value::Number(_)) => req.get("id").cloned().unwrap(),
            _ => continue,
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = match req.get("params") {
            Some(Value::Object(_)) => req.get("params").cloned().unwrap(),
            _ => json!({}),
        };
        let result = rt.block_on(dispatch(method, &params, config_path));
        let row = json!({"id": id, "result": result});
        if writeln!(out, "{row}").is_err() {
            std::process::exit(1);
        }
        if out.flush().is_err() {
            std::process::exit(1);
        }
    }
}
