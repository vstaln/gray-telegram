//! Telegram-side pairing. A user who messages an unconfigured bot (or one
//! they are not allowlisted on) is told their own user id and a one-shot
//! code; the owner approves the code with
//! `gray telegram pairing approve telegram <code>` and that user joins the
//! allowlist — or becomes the owner when none is set yet. `/start` counts
//! as the same hello, so binding a chat is just messaging the bot.

use serde_json::Value;
use std::path::Path;

/// A fresh code: 8 uppercase alphanumerics (short enough
/// to read off a phone, 36^8 space, single-use).
pub fn gen_code() -> String {
    const ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 8];
    crate::durable::fill_entropy(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// The reply an unconfigured DMer sees. Their own ID comes from Telegram
/// itself, so they cannot typo it; the code is single-use. HTML-escaped:
/// it is sent with parse_mode=HTML like every bridge message.
pub fn unconfigured_reply(user_id: &str, code: &str) -> String {
    format!(
        "Access not configured.\n\
         Your Telegram user id: <code>{user_id}</code>\n\
         Pairing code: <code>{code}</code>\n\
         Ask the bot owner to approve with:\n\
         <code>gray telegram pairing approve telegram {code}</code>"
    )
}

/// What the gateway does when a message is not admitted and the sender is a
/// human in a private chat: mint (or reuse) a code and reply. Bots, group
/// messages, and admitted senders never see this — `None` for them.
pub struct PairingReply {
    pub text: String,
    #[allow(dead_code)]
    pub code: String,
}

pub fn reply_for(
    store: &crate::durable::Store,
    author: &str,
    private_chat: bool,
    bot: bool,
) -> Option<PairingReply> {
    if bot || !private_chat || author.is_empty() {
        return None;
    }
    let code = store
        .pairing_code_for(author)
        .ok()
        .flatten()
        .unwrap_or_else(gen_code);
    store.pairing_insert(&code, author).ok()?;
    Some(PairingReply {
        text: unconfigured_reply(author, &code),
        code,
    })
}

/// Consume a code and admit its user: they become `owner_id` if the config
/// has none, otherwise they join `allowed_users`. Config is rewritten
/// atomically; the running gateway re-reads it only on restart, so the
/// answer tells the operator to restart.
pub fn approve(config_path: &Path, platform: &str, code: &str) -> Result<String, String> {
    if platform != "telegram" {
        return Err("Only the telegram platform exists here".to_string());
    }
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let store = crate::durable::Store::new(&parent.join("queue.sqlite"))?;
    let user = store
        .pairing_take(code.trim())?
        .ok_or_else(|| "unknown or already-used pairing code".to_string())?;
    let mut config = crate::config::load_config(config_path)?;
    let owner_missing = config
        .get("owner_id")
        .and_then(Value::as_str)
        .is_none_or(|o| o.is_empty());
    let note = if owner_missing {
        config["owner_id"] = Value::String(user.clone());
        // A fresh owner's own DM is also the home chat when none is set.
        if !config
            .get("chat_id")
            .is_some_and(crate::config::telegram_id)
        {
            config["chat_id"] = Value::String(user.clone());
        }
        format!("{user} is now the bot owner")
    } else {
        let mut allowed: Vec<String> = config
            .get("allowed_users")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if allowed.iter().any(|id| id == &user) {
            return Ok(format!("{user} was already allowed"));
        }
        allowed.push(user.clone());
        config["allowed_users"] = Value::Array(allowed.into_iter().map(Value::String).collect());
        format!("{user} can now message the bot")
    };
    crate::config::save_config(config_path, &config)?;
    Ok(format!(
        "{note}; restart the gateway to apply (it re-reads on start)"
    ))
}
