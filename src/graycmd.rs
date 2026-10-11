//! The `gray cmd` headless command surface (GRAY-CMD-CONTRACT.md).
//!
//! Slash names that are not one of the five built-ins resolve against a
//! cached `gray cmd list`; a hit runs `gray cmd run` in the right home and
//! the answer arrives as text or as an inline keyboard compiled from the
//! `gray.ui` document. Button presses come back as `gc|<tok>|<lid>|<idx>`
//! callback queries — the document is kept in the queue store keyed by its
//! token (TTL 1h, matching core) so the index can resolve to its value on a
//! platform whose callback payload caps at 64 bytes.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A `cmd` child answers within this window; a wedged one is killed.
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Registry rows marked `slow` (compaction, plugin calls, updates) get
/// this window instead — there is no platform deadline on a plain reply.
const SLOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Registry freshness: a good list lives 5 minutes; a failed probe is
/// remembered for 1 so `/typo` bursts cannot fork a child per message.
const REGISTRY_TTL: std::time::Duration = std::time::Duration::from_secs(300);
const REGISTRY_RETRY: std::time::Duration = std::time::Duration::from_secs(60);
/// Pending ui documents live an hour — core's own token TTL.
pub const DOC_TTL_SECS: f64 = 3600.0;
/// Telegram's callback_data cap; `gc|<tok>|<lid>|<idx>` must fit inside.
const CALLBACK_MAX: usize = 64;
/// setMyCommands shows at most this many entries.
const MENU_MAX: usize = 15;

/// One entry from `gray cmd list`.
#[derive(Debug, Clone)]
pub struct CmdSpec {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    /// `conversation` runs in the chat's gray home; `global` in the main one.
    pub scope: String,
    /// `text` and `interactive` dispatch; `local` is TUI-only.
    pub surface: String,
    /// Runs that can exceed the normal timeout.
    pub slow: bool,
}

/// A fetched registry snapshot.
#[derive(Debug, Clone)]
pub struct CommandList {
    pub commands: Vec<CmdSpec>,
}

impl CommandList {
    /// Parse the `{ok,result:{commands:[…]}}` reply; `None` for old binaries
    /// or anything off-shape (the caller then behaves as pre-`cmd`).
    pub fn from_reply(reply: &Value) -> Option<Self> {
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let commands = reply
            .pointer("/result/commands")?
            .as_array()?
            .iter()
            .filter_map(|c| {
                Some(CmdSpec {
                    name: c.get("name")?.as_str()?.to_string(),
                    description: c
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    aliases: c
                        .get("aliases")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    scope: c
                        .get("scope")
                        .and_then(Value::as_str)
                        .unwrap_or("conversation")
                        .to_string(),
                    surface: c
                        .get("surface")
                        .and_then(Value::as_str)
                        .unwrap_or("interactive")
                        .to_string(),
                    slow: c.get("slow").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect();
        Some(Self { commands })
    }

    /// The spec for `name` or one of its aliases, case-insensitive.
    pub fn lookup(&self, name: &str) -> Option<&CmdSpec> {
        self.commands.iter().find(|c| {
            c.name.eq_ignore_ascii_case(name)
                || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
        })
    }
}

/// Shared `cmd list` cache: one child spawn per TTL window, cloned out.
pub struct Registry {
    bin: PathBuf,
    /// The home `cmd list` runs in — the user's main gray home.
    home: PathBuf,
    cache: Mutex<Option<(std::time::Instant, Option<Arc<CommandList>>)>>,
}

impl Registry {
    pub fn new(bin: PathBuf, home: PathBuf) -> Self {
        Self {
            bin,
            home,
            cache: Mutex::new(None),
        }
    }

    /// The current command list, or `None` when gray predates `cmd`.
    /// A stale good list survives a failed refresh.
    pub async fn get(&self) -> Option<Arc<CommandList>> {
        let mut slot = self.cache.lock().await;
        if let Some((at, cached)) = slot.as_ref() {
            let ttl = if cached.is_some() {
                REGISTRY_TTL
            } else {
                REGISTRY_RETRY
            };
            if at.elapsed() < ttl {
                return cached.clone();
            }
        }
        let fresh = cmd_json(&self.bin, &self.home, &["list"])
            .await
            .ok()
            .and_then(|v| CommandList::from_reply(&v))
            .map(Arc::new);
        if fresh.is_none() {
            // Keep serving the previous good list; re-stamp so we retry
            // after REGISTRY_RETRY rather than per message.
            let keep = slot.as_ref().and_then(|(_, c)| c.clone());
            *slot = Some((std::time::Instant::now(), keep.clone()));
            return keep;
        }
        *slot = Some((std::time::Instant::now(), fresh.clone()));
        fresh
    }
}

/// One `gray cmd` call: `<bin> cmd <argv…>` with `GRAY_HOME=<home>`, same
/// process model as turns (env pass-through minus our own variables,
/// PATH rebuilt so provider plugins resolve, own process group for the
/// timeout kill). The reply is the single JSON object on stdout.
async fn cmd_json(bin: &Path, home: &Path, argv: &[&str]) -> Result<Value, String> {
    cmd_json_t(bin, home, argv, CMD_TIMEOUT).await
}

async fn cmd_json_t(
    bin: &Path,
    home: &Path,
    argv: &[&str],
    timeout: std::time::Duration,
) -> Result<Value, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("cmd").args(argv);
    cmd.current_dir(home);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(k_str) = k.to_str() else { continue };
        if k_str.starts_with("GRAY_")
            || k_str.starts_with("TELEGRAM_")
            || k_str.starts_with("OPENAI_")
        {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("GRAY_HOME", home);
    cmd.env(
        "PATH",
        crate::runner::child_path(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
            bin,
        ),
    );
    let child = cmd.spawn().map_err(|_| "cannot spawn gray".to_string())?;
    let pid = child.id().unwrap_or(0);
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => return Err("gray cmd failed".to_string()),
        Err(_) => {
            if pid != 0 {
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            return Err("gray cmd timed out".to_string());
        }
    };
    if out.stdout.len() > 4 * 1024 * 1024 {
        return Err("gray cmd reply too large".to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|_| "invalid gray cmd reply".to_string())
}

/// The `{ok, result}` payload or the `{ok:false, error}` pair.
enum Reply {
    Result(Value),
    Error { code: String, message: String },
}

fn envelope(v: Value) -> Result<Reply, String> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(Reply::Result(
            v.get("result").cloned().unwrap_or(Value::Null),
        ));
    }
    let e = v.get("error").cloned().unwrap_or(Value::Null);
    Ok(Reply::Error {
        code: e
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unavailable")
            .to_string(),
        message: e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("command failed")
            .to_string(),
    })
}

/// The home a command runs in: its own scope decides.
fn run_home(
    config: &Value,
    config_path: &Path,
    conversation: &str,
    scope: &str,
) -> Option<PathBuf> {
    if scope == "global" {
        return config
            .get("gray_home")
            .and_then(Value::as_str)
            .map(PathBuf::from);
    }
    config_path.parent().map(|base| {
        base.join("conversations")
            .join(crate::runner::hex_sha256(conversation.as_bytes()))
    })
}

fn gray_bin(config: &Value) -> Option<PathBuf> {
    let bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    (!bin.is_empty()).then(|| PathBuf::from(bin))
}

pub(crate) const FILE_PREFIX: &str = "FILE:";

fn pager_kb(token: &str, cur: usize, total: usize) -> Value {
    json!({"inline_keyboard": [[
        {"text": "◀ Prev", "callback_data": format!("pg|{token}|{}", cur.saturating_sub(1))},
        {"text": format!("Page {}/{}", cur + 1, total), "callback_data": format!("pg|{token}|{cur}")},
        {"text": "Next ▶", "callback_data": format!("pg|{token}|{}", (cur + 1).min(total.saturating_sub(1)))},
    ]]})
}

async fn send_result_text(
    rest: &crate::transport::Rest,
    store: &crate::durable::Store,
    chat_id: &str,
    user_id: &str,
    home: &Path,
    text: &str,
) {
    let limit = crate::transport::MAX_MESSAGE_LEN - 48;
    if text.trim().is_empty() {
        let _ = rest.send_text(chat_id, "Done.").await;
        return;
    }
    let body = md_to_html(&crate::text::sanitize(text));
    if crate::text::utf16_len(&body) <= limit {
        if let Err(e) = rest.send_text(chat_id, &body).await {
            eprintln!("[telegram] command reply failed in chat {chat_id}: {e}");
        }
        return;
    }
    let mut budget = limit - 24;
    let pages: Vec<String> = loop {
        let rendered: Vec<String> = crate::text::paginate_markdown(text, budget)
            .into_iter()
            .map(|page| md_to_html(&crate::text::sanitize(&page)))
            .collect();
        if rendered.iter().all(|p| crate::text::utf16_len(p) <= limit) {
            break rendered;
        }
        if budget <= 128 {
            break rendered
                .into_iter()
                .flat_map(|p| crate::text::split_message(&p, limit).unwrap_or_else(|_| vec![p]))
                .collect();
        }
        budget /= 2;
    };
    if pages.len() <= 1 {
        if let Err(e) = rest.send_text(chat_id, &body).await {
            eprintln!("[telegram] command reply failed in chat {chat_id}: {e}");
        }
        return;
    }
    let token = crate::durable::uuid_hex();
    let doc = json!({"kind": "pager", "pages": &pages});
    if store
        .cmd_doc_put(
            &token,
            &doc.to_string(),
            &home.to_string_lossy(),
            "pager",
            user_id,
            crate::durable::now_secs(),
        )
        .is_err()
    {
        let _ = rest.send_text(chat_id, &pages[0]).await;
        let dir = home.join("overflow");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(format!("reply-{token}.txt"));
        if std::fs::write(&file, text).is_ok() {
            let _ = rest.send_document(chat_id, &file, None).await;
        }
        return;
    }
    let kb = pager_kb(&token, 0, pages.len());
    if let Err(e) = rest.send_markup(chat_id, &pages[0], &kb).await {
        eprintln!("[telegram] command reply failed in chat {chat_id}: {e}");
    }
}

/// A `kind:ui` result: compile the document to an inline keyboard, park it
/// in the store under its token, and post text + buttons in one message.
#[allow(clippy::too_many_arguments)]
async fn send_ui(
    rest: &crate::transport::Rest,
    store: &crate::durable::Store,
    chat_id: &str,
    user_id: &str,
    home: &Path,
    name: &str,
    token: &str,
    document: &Value,
    fallback: &str,
) {
    let Some((body, markup)) = compile(document, token) else {
        // Buttons would not fit callback_data or the doc is off-shape:
        // the numbered-menu fallback is the component-less render.
        send_result_text(rest, store, chat_id, user_id, home, fallback).await;
        return;
    };
    let doc_json = serde_json::to_string(document).unwrap_or_default();
    let _ = store.cmd_doc_put(
        token,
        &doc_json,
        &home.to_string_lossy(),
        name,
        user_id,
        crate::durable::now_secs(),
    );
    let text = if body.trim().is_empty() {
        md_to_html(fallback)
    } else {
        md_to_html(&crate::text::sanitize(&body))
    };
    let send = rest.send_markup(chat_id, &text, &markup).await;
    if let Err(e) = send {
        eprintln!("[telegram] picker post failed in chat {chat_id}: {e}");
        let _ = store.cmd_doc_delete(token);
    }
}

/// Dispatch a `/name args…` Gray command (commands::BotCommand::Gray).
#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    registry: &Registry,
    config: &Value,
    config_path: &Path,
    store: &crate::durable::Store,
    rest: &crate::transport::Rest,
    chat_id: &str,
    user_id: &str,
    conversation: &str,
    name: &str,
    args: &str,
) {
    let Some(bin) = gray_bin(config) else { return };
    let spec = registry
        .get()
        .await
        .and_then(|list| list.lookup(name).cloned());
    if spec.as_ref().map(|s| s.surface.as_str()) == Some("local") {
        let _ = rest
            .send_text(
                chat_id,
                "That command only works in gray's local interface.",
            )
            .await;
        return;
    }
    let scope = spec
        .as_ref()
        .map(|s| s.scope.as_str())
        .unwrap_or("conversation");
    let Some(home) = run_home(config, config_path, conversation, scope) else {
        return;
    };
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&home);
    }

    let mut argv: Vec<&str> = vec!["run", name];
    if !args.trim().is_empty() {
        argv.push(args.trim());
    }
    let timeout = if spec.as_ref().map(|s| s.slow).unwrap_or(false) {
        SLOW_TIMEOUT
    } else {
        CMD_TIMEOUT
    };
    let reply = match cmd_json_t(&bin, &home, &argv, timeout).await {
        Ok(v) => v,
        Err(_) => {
            let _ = rest
                .send_text(chat_id, "Command unavailable right now; try again.")
                .await;
            return;
        }
    };
    match envelope(reply) {
        Err(_) => {
            let _ = rest
                .send_text(chat_id, "Command unavailable right now; try again.")
                .await;
        }
        Ok(Reply::Error { code, message }) => {
            let text = match code.as_str() {
                "local_only" => "That command only works in gray's local interface.".to_string(),
                "unknown_command" => "Unknown command.".to_string(),
                _ => crate::text::escape_html(&message),
            };
            let _ = rest.send_text(chat_id, &text).await;
        }
        Ok(Reply::Result(result)) => {
            match result.get("kind").and_then(Value::as_str) {
                Some("prompt") => {
                    let prompt = result.get("prompt").and_then(Value::as_str).unwrap_or("");
                    let fallback = result
                        .get("fallback_text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let capacity = config
                        .get("queue_capacity")
                        .and_then(Value::as_u64)
                        .unwrap_or(1000);
                    let id = format!("tg:cmd:{}", crate::durable::uuid_hex());
                    let text =
                        match store.enqueue(&id, chat_id, prompt, Some(conversation), capacity) {
                            Ok(true) if fallback.trim().is_empty() => "Working on it.".to_string(),
                            Ok(true) => crate::text::escape_html(fallback),
                            Ok(false) => "That turn is already queued.".to_string(),
                            Err(e) => crate::text::escape_html(&e),
                        };
                    let _ = rest.send_text(chat_id, &text).await;
                }
                Some("ui") => {
                    let token = result.get("token").and_then(Value::as_str).unwrap_or("");
                    let document = result.get("document").cloned().unwrap_or(Value::Null);
                    let fallback = result
                        .get("fallback_text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    send_ui(
                        rest, store, chat_id, user_id, &home, name, token, &document, fallback,
                    )
                    .await;
                }
                _ => {
                    // kind "text" (or a missing kind): the text render.
                    let text = result.get("text").and_then(Value::as_str).unwrap_or("");
                    send_result_text(rest, store, chat_id, user_id, &home, text).await;
                }
            }
        }
    }
}

/// One `gc|…` callback_query: decode, resolve against the stored document,
/// `cmd interact`, and re-render the picker message per the result.
pub async fn callback(
    config: &Value,
    store: &crate::durable::Store,
    rest: &crate::transport::Rest,
    cq: &Value,
    owner_id: &str,
    allowed: &[String],
) {
    let cb_id = cq.get("id").and_then(Value::as_str).unwrap_or("");
    let from_id = cq
        .pointer("/from/id")
        .and_then(Value::as_i64)
        .map(|i| i.to_string())
        .unwrap_or_default();
    let data = cq.get("data").and_then(Value::as_str).unwrap_or("");
    let Some(msg) = cq.get("message") else {
        let _ = rest.answer_callback(cb_id, None, false).await;
        return;
    };
    let (chat_id, message_id) = (
        msg.pointer("/chat/id")
            .and_then(Value::as_i64)
            .map(|i| i.to_string())
            .unwrap_or_default(),
        msg.get("message_id").and_then(Value::as_i64).unwrap_or(0),
    );
    // Toasts pop through answerCallbackQuery; every path answers once.
    let toast = |text: Option<&str>, alert: bool| {
        let rest = rest.clone();
        let cb_id = cb_id.to_string();
        let text = text.map(str::to_string);
        async move {
            let _ = rest.answer_callback(&cb_id, text.as_deref(), alert).await;
        }
    };

    if data.starts_with("pg|") {
        let mut parts = data.split('|');
        let (Some("pg"), Some(tok), Some(idx), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            toast(Some("Unknown action."), false).await;
            return;
        };
        if !crate::policy::is_allowed_user(&from_id, owner_id, allowed) {
            toast(Some("This pager is not for you."), true).await;
            return;
        }
        let row = match store.cmd_doc_get(tok, DOC_TTL_SECS, crate::durable::now_secs()) {
            Ok(Some(row)) => row,
            _ => {
                toast(Some("Pager expired — send the command again."), false).await;
                return;
            }
        };
        if row.owner != from_id {
            toast(Some("This pager belongs to someone else."), true).await;
            return;
        }
        let doc: Value = serde_json::from_str(&row.doc).unwrap_or(Value::Null);
        let pages: Vec<&str> = doc
            .get("pages")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(&page) = idx
            .parse::<usize>()
            .ok()
            .filter(|_| !pages.is_empty())
            .and_then(|i| pages.get(i.min(pages.len() - 1)))
        else {
            toast(Some("That page is no longer there."), false).await;
            return;
        };
        let cur = idx.parse::<usize>().unwrap_or(0).min(pages.len() - 1);
        let kb = pager_kb(tok, cur, pages.len());
        let _ = rest
            .edit_message(&chat_id, message_id, page, Some(&kb))
            .await;
        toast(None, false).await;
        return;
    }

    let mut parts = data.split('|');
    let (Some("gc"), Some(tok), Some(a), Some(b), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        toast(Some("Unknown action."), false).await;
        return;
    };
    if !crate::policy::is_allowed_user(&from_id, owner_id, allowed) {
        toast(Some("This picker is not for you."), true).await;
        return;
    }
    let now = crate::durable::now_secs();
    let row = match store.cmd_doc_get(tok, DOC_TTL_SECS, now) {
        Ok(Some(row)) => row,
        _ => {
            toast(Some("Picker expired — send the command again."), false).await;
            return;
        }
    };
    if row.owner != from_id {
        toast(Some("This picker belongs to someone else."), true).await;
        return;
    }
    let doc: Value = serde_json::from_str(&row.doc).unwrap_or(Value::Null);
    let Some(action) = action_for(&doc, a, b, &from_id) else {
        toast(Some("That option is no longer there."), false).await;
        return;
    };
    let Some(bin) = gray_bin(config) else {
        toast(Some("Command unavailable."), false).await;
        return;
    };
    // The home comes from the stored row — the same scope `run` used.
    let action_json = serde_json::to_string(&action).unwrap_or_default();
    let reply = match cmd_json(&bin, Path::new(&row.home), &["interact", tok, &action_json]).await {
        Ok(v) => v,
        Err(_) => {
            toast(Some("Command unavailable right now."), false).await;
            return;
        }
    };
    let empty_kb = json!({"inline_keyboard": []});
    match envelope(reply) {
        Err(_) => {
            toast(Some("Command failed."), false).await;
        }
        Ok(Reply::Error { code, message }) => match code.as_str() {
            "expired" => {
                let _ = store.cmd_doc_delete(tok);
                let _ = rest
                    .edit_message(&chat_id, message_id, "Picker expired.", Some(&empty_kb))
                    .await;
                toast(Some(&format!("picker expired, /{} again", row.name)), false).await;
            }
            "not_owner" => {
                toast(Some("This picker belongs to someone else."), true).await;
            }
            _ => {
                let short: String = message.chars().take(180).collect();
                toast(Some(&short), false).await;
            }
        },
        Ok(Reply::Result(result)) => match result.get("kind").and_then(Value::as_str) {
            Some("prompt") => {
                let prompt = result.get("prompt").and_then(Value::as_str).unwrap_or("");
                let fallback = result
                    .get("fallback_text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let is_private =
                    msg.pointer("/chat/type").and_then(Value::as_str) == Some("private");
                let conv = crate::session::conversation_key(&chat_id, &from_id, is_private);
                let capacity = config
                    .get("queue_capacity")
                    .and_then(Value::as_u64)
                    .unwrap_or(1000);
                let id = format!("tg:cmd:{}", crate::durable::uuid_hex());
                match store.enqueue(&id, &chat_id, prompt, Some(&conv), capacity) {
                    Ok(true) => {
                        let text = if fallback.trim().is_empty() {
                            "Working on it.".to_string()
                        } else {
                            crate::text::escape_html(fallback)
                        };
                        let _ = rest.send_text(&chat_id, &text).await;
                        toast(None, false).await;
                    }
                    Ok(false) => {
                        toast(Some("That turn is already queued."), false).await;
                    }
                    Err(e) => {
                        let short: String = e.chars().take(180).collect();
                        toast(Some(&short), false).await;
                    }
                }
            }
            Some("ui") => {
                let document = result.get("document").cloned().unwrap_or(Value::Null);
                // Editable docs keep their token, but if core ever hands a
                // fresh one the picker must re-key or the next press misses.
                let next_tok = result.get("token").and_then(Value::as_str).unwrap_or(tok);
                let fallback = result
                    .get("fallback_text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match compile(&document, next_tok) {
                    Some((body, markup)) => {
                        let text = if body.trim().is_empty() {
                            md_to_html(fallback)
                        } else {
                            md_to_html(&crate::text::sanitize(&body))
                        };
                        let doc_json = serde_json::to_string(&document).unwrap_or_default();
                        let _ = store.cmd_doc_put(
                            next_tok, &doc_json, &row.home, &row.name, &row.owner, now,
                        );
                        if next_tok != tok {
                            let _ = store.cmd_doc_delete(tok);
                        }
                        let _ = rest
                            .edit_message(&chat_id, message_id, &text, Some(&markup))
                            .await;
                        toast(None, false).await;
                    }
                    None => {
                        let _ = store.cmd_doc_delete(tok);
                        let _ = rest
                            .edit_message(
                                &chat_id,
                                message_id,
                                &md_to_html(fallback),
                                Some(&empty_kb),
                            )
                            .await;
                        toast(None, false).await;
                    }
                }
            }
            _ => {
                // kind text + close: the flow is done — show the text, drop
                // the keyboard, and free the stored document.
                let _ = store.cmd_doc_delete(tok);
                let text = result
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("Done.");
                let body = md_to_html(&crate::text::sanitize(text));
                let _ = rest
                    .edit_message(&chat_id, message_id, &body, Some(&empty_kb))
                    .await;
                toast(None, false).await;
            }
        },
    }
}

/// callback `gc|<tok>|<a>|<b>` → the contract action-json. `a` naming a
/// string_select in the document submits its b-th option's value; `a == "s"`
/// submits button `b`. Decoding prefers the select read so a select whose
/// logical_id is literally "s" still works.
fn action_for(doc: &Value, a: &str, b: &str, user: &str) -> Option<Value> {
    if let Some(value) = select_value(doc, a, b) {
        return Some(json!({
            "user": user,
            "values": {a: [value]},
        }));
    }
    if a == "s" && !b.is_empty() {
        return Some(json!({"user": user, "submit": b}));
    }
    None
}

/// The `value` of option `index` in the doc's `lid` string_select.
fn select_value(doc: &Value, lid: &str, index: &str) -> Option<String> {
    let idx: usize = index.parse().ok()?;
    for node in doc.get("components")?.as_array()? {
        let children = match node.get("type").and_then(Value::as_str) {
            Some("action_row") => node.get("children").and_then(Value::as_array),
            _ => continue,
        };
        for child in children.into_iter().flatten() {
            if child.get("type").and_then(Value::as_str) != Some("string_select")
                || child.get("logical_id").and_then(Value::as_str) != Some(lid)
            {
                continue;
            }
            let options = child.get("options")?.as_array()?;
            return options
                .get(idx)?
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    None
}

/// `gc|<tok>|<lid>|<arg>` — the 64-byte callback_data budget decides whether
/// a document can render as buttons at all.
fn cb_data(tok: &str, lid: &str, arg: &str) -> Option<String> {
    let data = format!("gc|{tok}|{lid}|{arg}");
    (data.len() <= CALLBACK_MAX).then_some(data)
}

/// `gray.ui` v1 → (markdown body, inline_keyboard markup). `None` means the
/// document cannot render on Telegram — the caller posts `fallback_text`.
fn compile(document: &Value, token: &str) -> Option<(String, Value)> {
    if token.is_empty() {
        return None;
    }
    let components = document.get("components")?.as_array()?;
    let mut body: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    for node in components {
        match node.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(content) = node.get("content").and_then(Value::as_str) {
                    body.push(content.to_string());
                }
            }
            Some("separator") => body.push("──────────".to_string()),
            Some("action_row") => {
                let Some(children) = node.get("children").and_then(Value::as_array) else {
                    continue;
                };
                let mut buttons: Vec<Value> = Vec::new();
                for child in children {
                    match child.get("type").and_then(Value::as_str) {
                        Some("string_select") => {
                            let lid = child
                                .get("logical_id")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let options = child
                                .get("options")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            // Telegram pickers are a vertical button list —
                            // one row per option.
                            for (i, opt) in options.iter().take(25).enumerate() {
                                let label = opt
                                    .get("label")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                if label.is_empty() {
                                    continue;
                                }
                                let label = match opt.get("description").and_then(Value::as_str) {
                                    Some(d) if !d.is_empty() => {
                                        truncate(&format!("{label} — {d}"), 80)
                                    }
                                    _ => truncate(&label, 80),
                                };
                                let label =
                                    if opt.get("default").and_then(Value::as_bool) == Some(true) {
                                        format!("• {label}")
                                    } else {
                                        label
                                    };
                                let data = cb_data(token, lid, &i.to_string())?;
                                rows.push(json!([{"text": label, "callback_data": data}]));
                            }
                        }
                        Some("button") => {
                            let lid = child
                                .get("logical_id")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let label = child
                                .get("label")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if label.is_empty() {
                                continue;
                            }
                            if child.get("style").and_then(Value::as_str) == Some("link") {
                                if let Some(url) = child.get("url").and_then(Value::as_str) {
                                    buttons.push(json!({"text": truncate(&label, 64), "url": url}));
                                }
                                continue;
                            }
                            let data = cb_data(token, "s", lid)?;
                            buttons
                                .push(json!({"text": truncate(&label, 64), "callback_data": data}));
                        }
                        _ => {}
                    }
                }
                if !buttons.is_empty() {
                    rows.push(Value::Array(buttons));
                }
            }
            _ => {}
        }
    }
    if rows.len() > 96 {
        return None;
    }
    Some((body.join("\n\n"), json!({"inline_keyboard": rows})))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max - 1).collect::<String>())
}

/// `/help` — registry-backed when `cmd list` answers, today's static text
/// otherwise (old binaries keep working unchanged).
pub fn help_text(list: Option<&CommandList>) -> String {
    let Some(list) = list else {
        return crate::commands::HELP.to_string();
    };
    let mut out = String::from("Commands:\n");
    out.push_str("  /new — start a fresh conversation\n");
    out.push_str("  /stop — cancel the running task\n");
    out.push_str("  /status — what the queue is doing\n");
    out.push_str("  /help — this text\n");
    let extra: Vec<&CmdSpec> = list
        .commands
        .iter()
        .filter(|c| c.surface != "local")
        .collect();
    if !extra.is_empty() {
        out.push_str("\ngray commands:\n");
        for c in extra {
            out.push_str(&format!(
                "  /{} — {}\n",
                crate::text::escape_html(&c.name),
                crate::text::escape_html(&c.description)
            ));
        }
    }
    out.push_str("\nAttach a photo or document and I will hand it to the agent.");
    out
}

/// The setMyCommands payload: the five built-ins plus the top registry
/// commands (Telegram shows at most ~15; local-only ones stay hidden).
pub fn command_menu(list: Option<&CommandList>) -> Vec<Value> {
    let mut menu = vec![
        json!({"command": "start", "description": "connect to the bot"}),
        json!({"command": "new", "description": "start a fresh conversation"}),
        json!({"command": "stop", "description": "cancel the running task"}),
        json!({"command": "status", "description": "queue depth and state"}),
        json!({"command": "help", "description": "what the bot can do"}),
    ];
    if let Some(list) = list {
        for c in &list.commands {
            if menu.len() >= MENU_MAX {
                break;
            }
            let valid = !c.name.is_empty()
                && c.name.len() <= 32
                && c.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if c.surface == "local" || !valid || menu.iter().any(|m| m["command"] == c.name) {
                continue;
            }
            let description: String = c.description.chars().take(256).collect();
            menu.push(json!({
                "command": c.name,
                "description": if description.is_empty() { c.name.clone() } else { description },
            }));
        }
    }
    menu
}

/// Register the visible command list at boot. Best-effort: an old binary
/// (no `cmd`) just leaves the built-in menu.
pub async fn register_commands(rest: &crate::transport::Rest, registry: &Registry) {
    let list = registry.get().await;
    if let Err(e) = rest.set_my_commands(&command_menu(list.as_deref())).await {
        eprintln!("[telegram] setMyCommands failed: {e}");
    }
}

/// `gray cmd` answers markdown; sendMessage speaks HTML. This is the small
/// common-denominator translation — fences, code spans, bold, italic,
/// links — not a CommonMark renderer; transport's plain-text retry is the
/// net for anything outside it.
pub fn md_to_html(md: &str) -> String {
    let mut out = String::new();
    let mut rest = md;
    while let Some(start) = rest.find("```") {
        out.push_str(&inline(&crate::text::escape_html(&rest[..start])));
        let after = &rest[start + 3..];
        // An optional language tag rides the opening fence line.
        let after = after.split_once('\n').map(|(_, r)| r).unwrap_or(after);
        match after.find("```") {
            Some(end) => {
                out.push_str("<pre>");
                out.push_str(&crate::text::escape_html(&after[..end]));
                out.push_str("</pre>");
                rest = &after[end + 3..];
            }
            None => {
                out.push_str("<pre>");
                out.push_str(&crate::text::escape_html(after));
                out.push_str("</pre>");
                rest = "";
            }
        }
    }
    out.push_str(&inline(&crate::text::escape_html(rest)));
    out
}

/// Inline markup on already-escaped text. Code spans are extracted to
/// placeholders first so `*` inside `` `code` `` stays literal.
fn inline(escaped: &str) -> String {
    let (s, codes) = extract_code(escaped);
    let s = delim(&s, "**", "<b>", "</b>", false);
    let s = delim(&s, "__", "<b>", "</b>", true);
    let s = delim(&s, "*", "<i>", "</i>", false);
    let s = delim(&s, "_", "<i>", "</i>", true);
    let s = links(&s);
    let mut out = s;
    for (i, code) in codes.iter().enumerate() {
        out = out.replace(&format!("\u{1}{i}\u{1}"), &format!("<code>{code}</code>"));
    }
    out
}

/// Pair `open`/`close` around `d`-delimited runs; an odd trailing mark stays
/// literal and never leaves a dangling tag. `wordy` delimiters (`_`) ignore
/// marks with alphanumerics on both sides so `snake_case` names survive.
fn delim(s: &str, d: &str, open: &str, close: &str, wordy: bool) -> String {
    let active = |at: usize| -> bool {
        if !wordy {
            return true;
        }
        let word = |c: Option<char>| c.map(|c| c.is_alphanumeric()).unwrap_or(false);
        !(word(s[..at].chars().last()) && word(s[at + d.len()..].chars().next()))
    };
    let mut marks: Vec<usize> = Vec::new();
    let mut scan = 0;
    while let Some(i) = s[scan..].find(d) {
        let at = scan + i;
        if active(at) {
            marks.push(at);
        }
        scan = at + d.len();
    }
    let pairs = marks.len() / 2 * 2;
    let mut out = String::new();
    let mut cursor = 0;
    for (n, at) in marks.iter().enumerate() {
        out.push_str(&s[cursor..*at]);
        out.push_str(if n < pairs {
            if n % 2 == 0 {
                open
            } else {
                close
            }
        } else {
            d
        });
        cursor = at + d.len();
    }
    out.push_str(&s[cursor..]);
    out
}

/// `` `x` `` → placeholder slots; restored after the other delimiters run.
fn extract_code(escaped: &str) -> (String, Vec<String>) {
    let mut codes = Vec::new();
    let mut out = String::new();
    let mut rest = escaped;
    while let Some(i) = rest.find('`') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        match tail.find('`') {
            Some(j) => {
                codes.push(tail[..j].to_string());
                out.push_str(&format!("\u{1}{}\u{1}", codes.len() - 1));
                rest = &tail[j + 1..];
            }
            None => {
                out.push('`');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    (out, codes)
}

/// `[label](url)` → `<a href>`. Anything half-formed stays literal.
fn links(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("](") {
        let Some(j) = rest[..i].rfind('[') else {
            out.push_str(&rest[..i + 2]);
            rest = &rest[i + 2..];
            continue;
        };
        let Some(k) = rest[i + 2..].find(')') else {
            break;
        };
        out.push_str(&rest[..j]);
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            &rest[i + 2..i + 2 + k],
            &rest[j + 1..i]
        ));
        rest = &rest[i + 2 + k + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmd list` fixture: what a current binary answers.
    fn list_reply() -> Value {
        json!({
            "ok": true, "protocol": 1,
            "result": {"commands": [
                {"name": "help", "description": "command help", "aliases": ["h"],
                 "args": [], "scope": "global", "surface": "text"},
                {"name": "model", "description": "pick the model", "aliases": ["m"],
                 "args": [{"name": "args", "required": false, "hint": "model id"}],
                 "scope": "conversation", "surface": "interactive"},
                {"name": "usage", "description": "tokens & cost", "aliases": [],
                 "args": [], "scope": "conversation", "surface": "text"},
                {"name": "quit", "description": "leave the TUI", "aliases": [],
                 "args": [], "scope": "global", "surface": "local"}
            ]}
        })
    }

    /// The model-picker document from the contract.
    fn model_picker() -> Value {
        json!({
            "protocol": "gray.ui", "version": 1, "surface": "message",
            "document_id": "model-picker", "visibility": "ephemeral",
            "lifecycle": "editable",
            "components": [
                {"type": "text", "content": "**Model** — `gpt-5` · effort `medium` · fast `off`"},
                {"type": "action_row", "children": [
                    {"type": "string_select", "logical_id": "model",
                     "options": [
                        {"label": "GPT 5", "value": "openai/gpt-5", "default": true},
                        {"label": "Claude", "value": "anthropic/claude", "description": "sonnet"}
                     ]}
                ]},
                {"type": "action_row", "children": [
                    {"type": "button", "logical_id": "apply", "label": "Apply", "style": "primary"},
                    {"type": "button", "logical_id": "fast", "label": "fast: off", "style": "secondary"},
                    {"type": "button", "logical_id": "cancel", "label": "Cancel", "style": "secondary"}
                ]}
            ]
        })
    }

    #[test]
    fn parses_command_list() {
        let list = CommandList::from_reply(&list_reply()).unwrap();
        assert_eq!(list.commands.len(), 4);
        assert_eq!(list.lookup("m").unwrap().name, "model");
        assert_eq!(list.lookup("MODEL").unwrap().name, "model");
        assert!(list.lookup("bogus").is_none());
        assert!(CommandList::from_reply(&json!({"ok": false})).is_none());
        assert!(CommandList::from_reply(&json!({"garbage": 1})).is_none());
    }

    #[test]
    fn compiles_picker_to_keyboard() {
        let (body, markup) = compile(&model_picker(), "tok123").unwrap();
        assert!(body.contains("**Model**"));
        let rows = markup["inline_keyboard"].as_array().unwrap();
        // 2 select options (one row each) + 1 row of 3 buttons.
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0][0]["callback_data"].as_str().unwrap(),
            "gc|tok123|model|0"
        );
        assert_eq!(
            rows[1][0]["callback_data"].as_str().unwrap(),
            "gc|tok123|model|1"
        );
        assert!(rows[0][0]["text"].as_str().unwrap().starts_with('•'));
        let buttons = rows[2].as_array().unwrap();
        assert_eq!(buttons.len(), 3);
        assert_eq!(
            buttons[0]["callback_data"].as_str().unwrap(),
            "gc|tok123|s|apply"
        );
    }

    #[test]
    fn decodes_callback_to_action() {
        let doc = model_picker();
        let select = action_for(&doc, "model", "1", "777").unwrap();
        assert_eq!(select["values"]["model"][0], "anthropic/claude");
        assert_eq!(select["user"], "777");
        let submit = action_for(&doc, "s", "apply", "777").unwrap();
        assert_eq!(submit["submit"], "apply");
        assert!(action_for(&doc, "bogus", "x", "777").is_none());
        assert!(action_for(&doc, "model", "9", "777").is_none());
    }

    #[test]
    fn oversized_callback_fails_compile() {
        let doc = model_picker();
        let long_tok = "t".repeat(60);
        assert!(compile(&doc, &long_tok).is_none());
    }

    #[test]
    fn markdown_renders_html() {
        assert_eq!(
            md_to_html("**Model** — `gpt-5` · effort `m`"),
            "<b>Model</b> — <code>gpt-5</code> · effort <code>m</code>"
        );
        assert_eq!(md_to_html("a_b_c"), "a_b_c");
        assert_eq!(md_to_html("_x_"), "<i>x</i>");
        assert_eq!(
            md_to_html("see [docs](https://x.dev/y)"),
            "see <a href=\"https://x.dev/y\">docs</a>"
        );
        assert_eq!(md_to_html("a < b"), "a &lt; b");
        assert_eq!(md_to_html("```\nx<y\n```"), "<pre>x&lt;y\n</pre>");
    }

    #[test]
    fn menu_fits_telegram_limits() {
        let list = CommandList::from_reply(&list_reply()).unwrap();
        let menu = command_menu(Some(&list));
        // 5 built-ins + model/usage (help dedupes, quit is local-only).
        assert_eq!(menu.len(), 7);
        assert!(menu.iter().all(|m| m["command"]
            .as_str()
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')));
        let fallback = command_menu(None);
        assert_eq!(fallback.len(), 5);
    }

    #[test]
    fn help_falls_back_without_registry() {
        assert_eq!(help_text(None), crate::commands::HELP);
        let list = CommandList::from_reply(&list_reply()).unwrap();
        let text = help_text(Some(&list));
        assert!(text.contains("/model — pick the model"));
        assert!(!text.contains("quit"));
    }
}
