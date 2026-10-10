//! Bot-token and identity checks for the setup wizard and doctor.
//! Failure kinds split into "the token is wrong" (re-prompt) and "Telegram
//! could not be reached" (keep the token, warn).
use serde_json::Value;

use crate::transport::{Rest, TransportError};

/// What a token check learned. Telegram exposes no "app owners" the way
/// Discord's application object does, so ownership is discovered by pairing
/// or typed in, not offered.
#[derive(Debug, Clone)]
pub struct BotCheck {
    /// The bot's numeric user id.
    pub id: String,
    /// `@username` without the `@`.
    pub username: String,
    /// Display name (first_name) for friendly setup output.
    pub name: String,
    /// Whether the bot may be added to groups at all (`can_join_groups`).
    pub joins_groups: bool,
    /// Whether privacy mode is on (`can_read_all_group_messages` false).
    pub privacy_mode: bool,
}

/// Why a token check failed.
#[derive(Debug)]
pub enum CheckError {
    /// Telegram rejected the token: the wizard should ask again.
    Invalid(String),
    /// Network failure or a Telegram 5xx: keep the token.
    Transient(String),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Invalid(e) | CheckError::Transient(e) => f.write_str(e),
        }
    }
}
impl std::error::Error for CheckError {}

/// `getMe` with a 10s bound; a bad token retries in the wizard, a timeout
/// is kept with a warning.
pub async fn check_bot_token(rest: &Rest) -> Result<BotCheck, CheckError> {
    let me = tokio::time::timeout(std::time::Duration::from_secs(10), rest.get_me())
        .await
        .map_err(|_| {
            CheckError::Transient("Telegram did not answer the token check".to_string())
        })?
        .map_err(|e| match e {
            TransportError::Auth(_) => {
                CheckError::Invalid("Telegram rejected the bot token".to_string())
            }
            other => CheckError::Transient(other.to_string()),
        })?;
    let id = me.get("id").and_then(Value::as_i64).unwrap_or(0).to_string();
    let username = me
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if username.is_empty() {
        return Err(CheckError::Invalid(
            "Telegram answered with no bot username".to_string(),
        ));
    }
    Ok(BotCheck {
        id,
        username,
        name: me
            .get("first_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        joins_groups: me
            .get("can_join_groups")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        privacy_mode: !me
            .get("can_read_all_group_messages")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Comma-separated user IDs, dropping non-numeric noise. Telegram user IDs
/// are positive integers; a chat ID may be negative, so this is the stricter
/// of the two validators.
pub fn clean_user_ids(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| crate::config::user_id(&Value::String(s.to_string())))
        .map(str::to_string)
        .collect()
}

/// Merge new IDs into an allowlist, preserving order and dropping dupes.
pub fn merge_allowed(existing: &[String], new: &[String]) -> Vec<String> {
    let mut out = existing.to_vec();
    for id in new {
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// A t.me link for the setup output ("open this in Telegram").
pub fn bot_link(username: &str) -> String {
    format!("https://t.me/{username}")
}
