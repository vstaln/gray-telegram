//! Interactive owner pairing.
//! Credentials stay in the wizard; the model never receives the token.
use serde_json::{json, Value};
use std::path::Path;

use crate::telegram_check::{self, BotCheck, CheckError};

/// Telegram rejections the wizard re-asks before giving up.
pub const TOKEN_TRIES: usize = 3;

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
}

/// Owner pairing result: `(owner_id, chat_id)` from the getUpdates wait.
pub type PairingResult = Result<(String, String), String>;
/// The pairing wait as a future: it holds a long-poll connection open, so it
/// cannot be plain sync work.
pub type PairingFut = std::pin::Pin<Box<dyn std::future::Future<Output = PairingResult> + Send>>;
/// Injected wait for the owner's message: receives the bot token and the
/// printed code, returns `(owner_id, chat_id)`. Tests inject a scripted
/// fake; the CLI passes [`default_pairing`].
pub type WaitForPairing = dyn Fn(&str, &str) -> PairingFut;
/// Token check injected for tests (`default_check` in production).
pub type CheckStep = dyn Fn(&str) -> CheckFut;

/// Real terminal prompter (hidden input via `stty`-style tcsetattr, no deps).
pub struct Tty;

impl Prompter for Tty {
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn prompt(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let res = without_echo(|| std::io::stdin().read_line(&mut line));
        let _ = std::io::stderr().write_all(b"\n");
        res.map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim().to_string())
    }
    fn confirm(&mut self, text: &str) -> Result<bool, String> {
        let answer = self.prompt(text)?.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
    fn print_line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// Echo off for one read. Ctrl-C would otherwise kill setup with the
/// terminal still silent, so SIGINT restores it first (tcsetattr is
/// async-signal-safe).
#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_and_exit(_: libc::c_int) {
        if let Some(t) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        unsafe { libc::_exit(130) };
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| term);
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let old = unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        libc::signal(libc::SIGINT, handler)
    };
    let out = read();
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        libc::signal(libc::SIGINT, old);
    }
    out
}

/// Echo off for one read via the console input mode. If setup is Ctrl-C'd
/// mid-read the console stays silent, but cmd.exe restores its own mode at
/// the next prompt.
#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return read();
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return read();
        }
        let saved = mode;
        SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT);
        let out = read();
        SetConsoleMode(handle, saved);
        out
    }
}

#[cfg(not(any(unix, windows)))]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    read()
}

/// Resolve the gray binary (`GRAY_BIN` env or `PATH` lookup).
fn resolve_gray(explicit: &str) -> Option<String> {
    let candidate = explicit.trim();
    if !candidate.is_empty() {
        let path = std::path::Path::new(candidate);
        if path.is_file() {
            return Some(candidate.to_string());
        }
    }
    which("gray")
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let mut candidates = vec![dir.join(name)];
        if cfg!(windows) && !name.ends_with(".exe") {
            candidates.push(dir.join(format!("{name}.exe")));
        }
        for p in candidates {
            if p.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if p.metadata().ok()?.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// The wizard inputs gathered before anything is written.
pub struct Wiring<'a> {
    pub check: &'a CheckStep,
    pub wait: &'a WaitForPairing,
    pub verify: &'a VerifyStep,
}

/// Doctor call injected for tests.
pub type VerifyStep = dyn Fn(&Value) -> VerifyFut;
/// Boxed verify future: `Send` so the wizard stays `Send` for tokio.
pub type VerifyFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

/// The production doctor, bounded like the CLI's own (30 s).
pub fn default_verify(config: &Value) -> VerifyFut {
    let config = config.clone();
    Box::pin(async move {
        let token = crate::config::token(&config).unwrap_or_default();
        let rest = crate::transport::Rest::production(&token);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            crate::doctor::check(&config, &rest),
        )
        .await
        .map_err(|_| "doctor timed out after 30s".to_string())?
    })
}

/// Boxed check future: `Send` so the wizard stays `Send` for tokio.
pub type CheckFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<BotCheck, CheckError>> + Send>>;

/// Production check: `getMe` with the bot token.
pub fn default_check(token: &str) -> CheckFut {
    let token = token.to_string();
    Box::pin(async move {
        let rest = crate::transport::Rest::production(&token);
        telegram_check::check_bot_token(&rest).await
    })
}

/// The production pairing wait: long-polls getUpdates for a private message
/// carrying the printed code, then stops. The code expires after 300 s.
/// Returns `(user_id, chat_id)` of whoever sent it.
pub fn default_pairing(token: &str, code: &str) -> PairingFut {
    let token = token.to_string();
    let code = code.to_string();
    Box::pin(async move {
        let rest = crate::transport::Rest::production(&token);
        // Start past any backlog so only a fresh code-bearing DM counts.
        let mut offset: i64 = rest
            .get_updates(0)
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|u| u.get("update_id").and_then(Value::as_i64))
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        let deadline = std::time::Duration::from_secs(300);
        let started = std::time::Instant::now();
        loop {
            let left = deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            let poll = tokio::time::timeout(
                left.min(std::time::Duration::from_secs(
                    crate::transport::LONG_POLL_SECS + 15,
                )),
                rest.get_updates(offset),
            )
            .await;
            let updates = match poll {
                Ok(Ok(u)) => u,
                Ok(Err(_)) | Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };
            for update in updates {
                if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                    offset = offset.max(id + 1);
                }
                let Some(msg) = update.get("message") else {
                    continue;
                };
                let private = msg
                    .get("chat")
                    .and_then(|c| c.get("type"))
                    .and_then(Value::as_str)
                    == Some("private");
                let bot = msg
                    .get("from")
                    .and_then(|f| f.get("is_bot"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let text = msg.get("text").and_then(Value::as_str).unwrap_or("");
                if private && !bot && crate::policy::code_matches(text, &code) {
                    let user = msg
                        .get("from")
                        .and_then(|f| f.get("id"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        .to_string();
                    let chat = msg
                        .get("chat")
                        .and_then(|c| c.get("id"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        .to_string();
                    if user != "0" && chat != "0" {
                        return Ok((user, chat));
                    }
                }
            }
        }
        Err("Pairing failed or expired; send the code to the bot in a Telegram DM".to_string())
    })
}

/// Every comma-separated ID must be a positive Telegram user id.
fn anyhow_ids(ids: &[String]) -> Result<(), String> {
    if ids.is_empty() {
        return Err("At least your own user ID is required".to_string());
    }
    if ids
        .iter()
        .any(|id| !crate::config::user_id(&Value::String(id.clone())))
    {
        return Err("User IDs must be positive Telegram user IDs".to_string());
    }
    Ok(())
}

/// Ask for the token until Telegram accepts it or three bad ones in a row
/// force a bail. `TELEGRAM_BOT_TOKEN` is offered first and never echoed.
async fn ask_token(
    io: &mut dyn Prompter,
    wiring: &Wiring<'_>,
) -> Result<(String, Option<BotCheck>), String> {
    let mut rejected = 0usize;
    loop {
        let env = std::env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let token = match env {
            Some(t) => {
                io.print_line("Using TELEGRAM_BOT_TOKEN from the environment.");
                t
            }
            None => io.prompt_hidden("Telegram bot token (from @BotFather; input is hidden): ")?,
        };
        if token.is_empty() {
            return Err("No bot token supplied".to_string());
        }
        match (wiring.check)(&token).await {
            Ok(bot) => return Ok((token, Some(bot))),
            Err(CheckError::Invalid(why)) => {
                // A bad env token is still a bad token; ask instead of
                // spinning on it.
                if std::env::var("TELEGRAM_BOT_TOKEN").is_ok() {
                    io.print_line(&format!("{why}; ignoring the environment token."));
                    // Removing the var only affects this process's later
                    // reads; the token itself is not printed anywhere.
                    std::env::remove_var("TELEGRAM_BOT_TOKEN");
                }
                rejected += 1;
                if rejected >= TOKEN_TRIES {
                    return Err(
                        "Telegram rejected three tokens in a row; nothing was saved.".to_string(),
                    );
                }
                io.print_line(&why);
            }
            Err(CheckError::Transient(e)) => {
                io.print_line(&format!(
                    "Couldn't reach Telegram to verify the token ({e}); keeping it anyway."
                ));
                return Ok((token, None));
            }
        }
    }
}

/// The whole wizard. Returns `true` when a config was written.
pub async fn run(config_path: &Path, io: &mut dyn Prompter, pair: bool) -> Result<bool, String> {
    run_with(
        config_path,
        io,
        pair,
        &Wiring {
            check: &default_check,
            wait: &default_pairing,
            verify: &default_verify,
        },
    )
    .await
}

/// Wizard with injectable IO and Telegram/network steps (tests drive fakes).
pub async fn run_with(
    config_path: &Path,
    io: &mut dyn Prompter,
    pair: bool,
    wiring: &Wiring<'_>,
) -> Result<bool, String> {
    if !io.is_terminal() {
        return Err("setup needs a terminal".to_string());
    }
    if config_path.exists() && !io.confirm("A config exists here; overwrite it? [y/N] ")? {
        return Ok(false);
    }

    let (token, bot) = ask_token(io, wiring).await?;
    if let Some(bot) = &bot {
        io.print_line(&format!(
            "Connected as @{} ({}). Open {} in Telegram.",
            bot.username,
            bot.name,
            telegram_check::bot_link(&bot.username)
        ));
        if bot.privacy_mode {
            io.print_line(
                "Note: privacy mode is on — in groups the bot only sees commands, \
                 mentions of itself, and replies to its own messages.",
            );
        }
    }

    // Owner + home chat. `--pair` discovers both from a DM carrying a
    // one-shot code; the default asks for a numeric user id.
    let (owner_id, chat_id) = if pair {
        let code = crate::pairing::gen_code();
        let username = bot
            .as_ref()
            .map(|b| b.username.clone())
            .unwrap_or_else(|| "your bot".to_string());
        io.print_line(&format!(
            "Open a private chat with @{username} in Telegram and send it exactly this code:\n  {code}\nWaiting 5 minutes…"
        ));
        match (wiring.wait)(&token, &code).await {
            Ok((user, chat)) => {
                io.print_line(&format!("Paired: user {user}, chat {chat}."));
                (user, chat)
            }
            Err(e) => return Err(e),
        }
    } else {
        io.print_line(
            "Your Telegram user ID is a number — send the bot a message and run \
             `gray telegram pairing approve` later, or check @userinfobot.",
        );
        let owner = io.prompt("Your Telegram user ID (Enter to leave unpaired): ")?;
        let owner = owner.trim().to_string();
        if owner.is_empty() {
            // Nobody admitted yet: strangers get pairing replies until the
            // operator approves a code. A home chat is still required.
            let chat = io.prompt("Home chat ID for notifications: ")?;
            let chat = chat.trim().to_string();
            if !crate::config::telegram_id(&Value::String(chat.clone())) {
                return Err("chat_id must be a Telegram chat ID".to_string());
            }
            (String::new(), chat)
        } else {
            anyhow_ids(std::slice::from_ref(&owner))?;
            let chat = io
                .prompt(&format!("Home chat ID [{owner}]: "))?
                .trim()
                .to_string();
            let chat = if chat.is_empty() { owner.clone() } else { chat };
            if !crate::config::telegram_id(&Value::String(chat.clone())) {
                return Err("chat_id must be a Telegram chat ID".to_string());
            }
            (owner, chat)
        }
    };

    let extra = io.prompt("Other allowed user IDs (comma-separated, Enter to skip): ")?;
    let mut allowed: Vec<String> = Vec::new();
    if !owner_id.is_empty() {
        allowed.push(owner_id.clone());
    }
    allowed = telegram_check::merge_allowed(&allowed, &telegram_check::clean_user_ids(&extra));

    let gray_bin = match resolve_gray(&std::env::var("GRAY_BIN").unwrap_or_default()) {
        Some(bin) => bin,
        None => {
            let typed = io.prompt("Absolute path to the gray binary: ")?;
            let typed = typed.trim().to_string();
            if !std::path::Path::new(&typed).is_file() {
                return Err("gray executable is missing or not executable".to_string());
            }
            typed
        }
    };
    let default_home = dirs_home().join(".gray");
    let gray_home = if default_home.join("config.json").is_file() {
        default_home.to_string_lossy().into_owned()
    } else {
        let typed = io.prompt(&format!(
            "gray home directory [{}]: ",
            default_home.display()
        ))?;
        let typed = typed.trim();
        if typed.is_empty() {
            default_home.to_string_lossy().into_owned()
        } else {
            absolutize(Path::new(&shellexpand_home(typed)))
                .to_string_lossy()
                .into_owned()
        }
    };

    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let workdir = parent.join("work").to_string_lossy().into_owned();

    let mut config = json!({
        "token": token,
        "chat_id": chat_id,
        "allowed_users": allowed,
        "workdir": workdir,
        "gray_bin": gray_bin,
        "gray_home": gray_home,
        "restart_notification": true,
        "typing_indicator": true,
        "concurrency": 2,
        "timeout_seconds": 600,
        "queue_capacity": 1000,
        "rate_limit_capacity": 8,
        "rate_limit_window_secs": 60,
        "max_attachment_bytes": 8388608
    });
    if !owner_id.is_empty() {
        config["owner_id"] = Value::String(owner_id);
    }
    crate::config::save_config(config_path, &config)?;
    io.print_line(&format!("Config written to {}", config_path.display()));

    match (wiring.verify)(&config).await {
        Ok(()) => {}
        Err(e) => io.print_line(&format!("Post-setup check failed: {e}")),
    }
    Ok(true)
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Join with cwd when relative (no `canonicalize`: the target may not
/// exist yet).
fn absolutize(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn shellexpand_home(raw: &str) -> String {
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).to_string_lossy().into_owned();
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", Path::new(&home).to_string_lossy());
        }
    }
    raw.to_string()
}
