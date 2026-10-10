//! Doctor preflight checks with safe errors.
//! Every failure is a category string; transport/config bodies never surface.
use serde_json::Value;

use crate::transport::Rest;

/// Production entry point: checks `config` against the live Telegram API.
pub async fn doctor(config: &Value) -> Result<(), String> {
    let token = crate::config::token(config).ok_or_else(|| "Bot token is missing".to_string())?;
    let rest = Rest::production(&token);
    check(config, &rest).await
}

/// All checks, with the REST client injected so tests can use a stub.
/// Order: gray binary → provider model → token (getMe) → home chat reach
/// (getChat). Telegram has no intent toggle or channel-permission ladder —
/// a bot either sees a chat or gets a 403/400, which getChat surfaces.
pub async fn check(config: &Value, rest: &Rest) -> Result<(), String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if !is_executable(gray_bin) {
        return Err("gray executable is missing or not executable".to_string());
    }
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(std::path::Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "gray provider configuration is missing or invalid".to_string())?;
    if provider
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("gray has no configured model".to_string());
    }
    let me = rest.get_me().await.map_err(|e| e.to_string())?;
    let bot = me
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("(unnamed)");
    let chat_id = config
        .get("chat_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    rest.get_chat(chat_id)
        .await
        .map_err(|_| "home chat is not reachable — has it messaged the bot once?".to_string())?;
    println!("Bot @{bot}, home chat access and local gray configuration verified.");
    println!("Provider generation and a live message round-trip were not tested.");
    Ok(())
}

/// `os.access(path, X_OK)` equivalent: a real file with any exec bit set.
fn is_executable(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
