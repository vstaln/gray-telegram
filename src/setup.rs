//! Interactive owner pairing.
//! Credentials stay in the wizard; the model never receives the token.
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

use crate::telegram_check::{self, BotCheck, CheckError};

/// Telegram rejections the wizard re-asks before giving up.
pub const TOKEN_TRIES: usize = 3;

const SETUP_URL: &str = "https://setup.gray.alignment.id";
const CREATE_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_TIMEOUT: Duration = Duration::from_secs(35);
const OFFLINE_TRIES: u32 = 5;
const WAITING: &str = "Waiting for you to open the link in Telegram…";
const OPENED: &str = "Opened in Telegram, confirm the new bot there";
const HELLO: &str = "Gray is connected. Message me here.";

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
    fn show_status(&mut self, text: &str) {
        self.print_line(text);
    }
    fn status_done(&mut self) {}
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
    fn show_status(&mut self, text: &str) {
        use std::io::Write;
        let mut err = std::io::stderr();
        let _ = write!(err, "\r\x1b[K{text}");
        let _ = err.flush();
    }
    fn status_done(&mut self) {
        use std::io::Write;
        let _ = std::io::stderr().write_all(b"\n");
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

#[derive(Clone, Copy)]
pub enum OwnerSource {
    PairingCode,
    UserIds,
}

#[derive(Clone, Copy)]
pub enum SetupPath {
    Ask(OwnerSource),
    Create,
    TokenStdin(OwnerSource),
}

#[derive(Clone, Copy)]
enum TokenSource {
    Interactive,
    Stdin,
}

struct Identity {
    token: String,
    owner_id: String,
    chat_id: String,
    allowed: Vec<String>,
}

enum Phase {
    AskPath(OwnerSource),
    Create,
    Token {
        owner: OwnerSource,
        source: TokenSource,
    },
    Configure(Identity),
}

async fn ask_token(
    io: &mut dyn Prompter,
    wiring: &Wiring<'_>,
    source: TokenSource,
) -> Result<(String, Option<BotCheck>), String> {
    let mut rejected = 0usize;
    loop {
        let token = match source {
            TokenSource::Stdin => io.prompt("")?,
            TokenSource::Interactive => match std::env::var("TELEGRAM_BOT_TOKEN")
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
            {
                Some(t) => {
                    io.print_line("Using TELEGRAM_BOT_TOKEN from the environment.");
                    t
                }
                None => {
                    io.prompt_hidden("Telegram bot token (from @BotFather; input is hidden): ")?
                }
            },
        };
        let token = token.trim().to_string();
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

fn report_bot(io: &mut dyn Prompter, bot: &Option<BotCheck>) {
    let Some(bot) = bot else { return };
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

fn ask_user_ids(io: &mut dyn Prompter) -> Result<(String, String), String> {
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
        Ok((String::new(), chat))
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
        Ok((owner, chat))
    }
}

async fn pair_owner(
    io: &mut dyn Prompter,
    wiring: &Wiring<'_>,
    token: &str,
    bot: Option<&BotCheck>,
) -> Result<(String, String), String> {
    let code = crate::pairing::gen_code();
    let username = bot
        .map(|b| b.username.clone())
        .unwrap_or_else(|| "your bot".to_string());
    io.print_line(&format!(
        "Open a private chat with @{username} in Telegram and send it exactly this code:\n  {code}\nWaiting 5 minutes…"
    ));
    match (wiring.wait)(token, &code).await {
        Ok((user, chat)) => {
            io.print_line(&format!("Paired: user {user}, chat {chat}."));
            Ok((user, chat))
        }
        Err(e) => Err(e),
    }
}

async fn token_identity(
    io: &mut dyn Prompter,
    wiring: &Wiring<'_>,
    owner: OwnerSource,
    source: TokenSource,
) -> Result<Identity, String> {
    let (token, bot) = ask_token(io, wiring, source).await?;
    report_bot(io, &bot);
    let (owner_id, chat_id) = match owner {
        OwnerSource::PairingCode => pair_owner(io, wiring, &token, bot.as_ref()).await?,
        OwnerSource::UserIds => ask_user_ids(io)?,
    };
    let extra = io.prompt("Other allowed user IDs (comma-separated, Enter to skip): ")?;
    let mut allowed: Vec<String> = Vec::new();
    if !owner_id.is_empty() {
        allowed.push(owner_id.clone());
    }
    allowed = telegram_check::merge_allowed(&allowed, &telegram_check::clean_user_ids(&extra));
    Ok(Identity {
        token,
        owner_id,
        chat_id,
        allowed,
    })
}

fn ask_path(io: &mut dyn Prompter, owner: OwnerSource) -> Result<Phase, String> {
    loop {
        let answer = io.prompt(
            "How should the bot be created?\n  \
             1) Create a new bot in Telegram (recommended)\n  \
             2) Use a bot token from BotFather\nChoice [1]: ",
        )?;
        match answer.trim() {
            "" | "1" => return Ok(Phase::Create),
            "2" => {
                return Ok(Phase::Token {
                    owner,
                    source: TokenSource::Interactive,
                })
            }
            _ => io.print_line("Answer 1 or 2."),
        }
    }
}

fn setup_base() -> String {
    std::env::var("GRAY_TELEGRAM_SETUP_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| SETUP_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn setup_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(POLL_TIMEOUT)
        .build()
        .map_err(|_| "cannot start the setup client".to_string())
}

struct Pairing {
    pairing_id: String,
    poll_token: String,
    deep_link: String,
    qr_payload: String,
}

impl Pairing {
    fn from_json(v: &Value) -> Option<Self> {
        let deep_link = v.get("deep_link")?.as_str()?.to_string();
        let qr_payload = match v.get("qr_payload").and_then(Value::as_str) {
            Some(payload) if !payload.is_empty() => payload.to_string(),
            _ => deep_link.clone(),
        };
        Some(Self {
            pairing_id: v.get("pairing_id")?.as_str()?.to_string(),
            poll_token: v.get("poll_token")?.as_str()?.to_string(),
            deep_link,
            qr_payload,
        })
    }
}

struct BotReady {
    token: String,
    owner_id: i64,
    bot_username: String,
}

impl BotReady {
    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            token: v.get("token")?.as_str()?.to_string(),
            owner_id: v.get("owner_id")?.as_i64()?,
            bot_username: v
                .get("bot_username")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }
}

enum RequestFail {
    RateLimited(u64),
    Unreachable(String),
}

enum Poll {
    Waiting,
    Opened,
    Ready(BotReady),
    Expired,
    Rejected(String),
    Offline(String),
}

enum Wait {
    Ready(BotReady),
    Restart,
    GiveUp,
    Cancelled,
}

enum Creation {
    Bot(Identity),
    Fallback,
}

fn qr_lines(payload: &str) -> Result<Vec<String>, String> {
    let code = qrcode::QrCode::new(payload.as_bytes())
        .map_err(|_| "The setup link is too long to draw as a QR code.".to_string())?;
    let width = code.width();
    let modules = code.to_colors();
    let quiet = 2usize;
    let total = width + quiet * 2;
    let dark = |x: usize, y: usize| {
        x >= quiet
            && y >= quiet
            && x - quiet < width
            && y - quiet < width
            && modules[(y - quiet) * width + (x - quiet)] == qrcode::Color::Dark
    };
    let mut lines = Vec::new();
    for row in (0..total).step_by(2) {
        let mut line = String::with_capacity(total * 8);
        for x in 0..total {
            line.push_str(match (dark(x, row), dark(x, row + 1)) {
                (true, true) => "\x1b[30;40m█",
                (true, false) => "\x1b[30;47m▀",
                (false, true) => "\x1b[30;47m▄",
                (false, false) => "\x1b[37;47m█",
            });
        }
        line.push_str("\x1b[0m");
        lines.push(line);
    }
    Ok(lines)
}

async fn request_pairing(client: &reqwest::Client, base: &str) -> Result<Pairing, RequestFail> {
    let reply = client
        .post(format!("{base}/v1/pairings"))
        .timeout(CREATE_TIMEOUT)
        .send()
        .await
        .map_err(|_| RequestFail::Unreachable("the setup service did not answer".to_string()))?;
    let status = reply.status().as_u16();
    let retry_after = reply
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(600);
    match status {
        201 => reply
            .json::<Value>()
            .await
            .ok()
            .as_ref()
            .and_then(Pairing::from_json)
            .ok_or_else(|| {
                RequestFail::Unreachable("the setup service sent an unreadable reply".to_string())
            }),
        429 => Err(RequestFail::RateLimited(retry_after)),
        other => Err(RequestFail::Unreachable(format!(
            "the setup service answered HTTP {other}"
        ))),
    }
}

async fn poll_once(client: &reqwest::Client, base: &str, pairing: &Pairing) -> Poll {
    let reply = client
        .get(format!("{base}/v1/pairings/{}", pairing.pairing_id))
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", pairing.poll_token),
        )
        .timeout(POLL_TIMEOUT)
        .send()
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => {
            return Poll::Offline(if e.is_timeout() {
                "the poll timed out".to_string()
            } else {
                "the network dropped the poll".to_string()
            })
        }
    };
    match reply.status().as_u16() {
        200 => match reply.json::<Value>().await {
            Ok(body) => match BotReady::from_json(&body) {
                Some(ready) => Poll::Ready(ready),
                None => Poll::Offline("the setup service sent an unreadable reply".to_string()),
            },
            Err(_) => Poll::Offline("the setup service sent an unreadable reply".to_string()),
        },
        202 => match reply.json::<Value>().await {
            Ok(body) if body.get("status").and_then(Value::as_str) == Some("opened") => {
                Poll::Opened
            }
            Ok(_) => Poll::Waiting,
            Err(_) => Poll::Offline("the setup service sent an unreadable reply".to_string()),
        },
        404 | 410 => Poll::Expired,
        401 => Poll::Rejected("The setup service rejected this link; run setup again.".to_string()),
        other => Poll::Offline(format!("the setup service answered HTTP {other}")),
    }
}

async fn wait_for_bot(
    io: &mut dyn Prompter,
    client: &reqwest::Client,
    base: &str,
    pairing: &Pairing,
) -> Wait {
    let mut offline = 0u32;
    loop {
        let outcome = tokio::select! {
            outcome = poll_once(client, base, pairing) => outcome,
            _ = tokio::signal::ctrl_c() => {
                io.status_done();
                return Wait::Cancelled;
            }
        };
        match outcome {
            Poll::Waiting => {
                offline = 0;
                io.show_status(WAITING);
            }
            Poll::Opened => {
                offline = 0;
                io.show_status(OPENED);
            }
            Poll::Ready(ready) => {
                let done = if ready.bot_username.is_empty() {
                    "Done.".to_string()
                } else {
                    format!("Done: @{}.", ready.bot_username)
                };
                io.show_status(&done);
                io.status_done();
                return Wait::Ready(ready);
            }
            Poll::Expired => {
                io.status_done();
                io.print_line("That setup link expired.");
                return match io.confirm("Start over with a new link? [Y/n] ") {
                    Ok(true) => Wait::Restart,
                    Ok(false) => Wait::GiveUp,
                    Err(_) => Wait::GiveUp,
                };
            }
            Poll::Rejected(why) => {
                io.status_done();
                io.print_line(&why);
                return Wait::GiveUp;
            }
            Poll::Offline(why) => {
                offline += 1;
                if offline >= OFFLINE_TRIES {
                    io.status_done();
                    io.print_line(&format!("Lost the Gray Setup service ({why})."));
                    return Wait::GiveUp;
                }
                io.show_status(&format!("Reconnecting to the Gray Setup service ({why})…"));
                tokio::time::sleep(Duration::from_secs(1u64 << offline.min(3))).await;
            }
        }
    }
}

async fn confirm_new_bot(
    io: &mut dyn Prompter,
    wiring: &Wiring<'_>,
    ready: BotReady,
) -> Result<Creation, String> {
    if ready.owner_id <= 0 {
        io.print_line(
            "The setup service returned no owner for the new bot; \
             using a BotFather token instead.",
        );
        return Ok(Creation::Fallback);
    }
    let bot = match (wiring.check)(&ready.token).await {
        Ok(bot) => Some(bot),
        Err(CheckError::Transient(e)) => {
            io.print_line(&format!(
                "Couldn't reach Telegram to verify the new bot ({e}); keeping it anyway."
            ));
            None
        }
        Err(CheckError::Invalid(why)) => {
            io.print_line(&format!(
                "Telegram rejected the new bot token ({why}); using a BotFather token instead."
            ));
            return Ok(Creation::Fallback);
        }
    };
    report_bot(io, &bot);
    let owner = ready.owner_id.to_string();
    Ok(Creation::Bot(Identity {
        token: ready.token,
        owner_id: owner.clone(),
        chat_id: owner.clone(),
        allowed: vec![owner],
    }))
}

async fn create_bot(
    io: &mut dyn Prompter,
    wiring: &Wiring<'_>,
    base: &str,
) -> Result<Creation, String> {
    let client = setup_client()?;
    loop {
        let pairing = match request_pairing(&client, base).await {
            Ok(pairing) => pairing,
            Err(RequestFail::RateLimited(secs)) => {
                io.print_line(&format!(
                    "The Gray Setup service is rate limited right now ({secs}s to wait); \
                     using a BotFather token instead."
                ));
                return Ok(Creation::Fallback);
            }
            Err(RequestFail::Unreachable(why)) => {
                io.print_line(&format!(
                    "Couldn't create a bot through the Gray Setup service ({why}); \
                     using a BotFather token instead."
                ));
                return Ok(Creation::Fallback);
            }
        };
        match qr_lines(&pairing.qr_payload) {
            Ok(lines) => {
                io.print_line("Scan this code with your phone:");
                for line in lines {
                    io.print_line(&line);
                }
            }
            Err(why) => io.print_line(&why),
        }
        io.print_line(&format!("Or open this link: {}", pairing.deep_link));
        io.print_line("Open it on the phone or computer where you use Telegram.");
        io.show_status(WAITING);
        match wait_for_bot(io, &client, base, &pairing).await {
            Wait::Ready(ready) => return confirm_new_bot(io, wiring, ready).await,
            Wait::Restart => io.print_line("Starting over with a new link."),
            Wait::GiveUp => return Ok(Creation::Fallback),
            Wait::Cancelled => return Err("Setup cancelled; nothing was saved.".to_string()),
        }
    }
}

async fn configure(
    config_path: &Path,
    io: &mut dyn Prompter,
    identity: Identity,
    wiring: &Wiring<'_>,
) -> Result<bool, String> {
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

    let model = io.prompt("Model for Telegram (Enter to use the same model as terminal Gray): ")?;
    let model = model.trim().to_string();

    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let workdir = parent.join("work").to_string_lossy().into_owned();

    let mut config = json!({
        "token": identity.token,
        "chat_id": identity.chat_id,
        "allowed_users": identity.allowed,
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
    if !identity.owner_id.is_empty() {
        config["owner_id"] = Value::String(identity.owner_id);
    }
    if !model.is_empty() {
        config["model"] = Value::String(model);
    }
    crate::config::save_config(config_path, &config)?;
    io.print_line(&format!("Config written to {}", config_path.display()));

    match (wiring.verify)(&config).await {
        Ok(()) => {}
        Err(e) => io.print_line(&format!("Post-setup check failed: {e}")),
    }
    Ok(true)
}

pub async fn send_hello(config: &Value) -> Result<(), String> {
    let token = crate::config::token(config).ok_or_else(|| "Bot token is missing".to_string())?;
    let chat = config
        .get("chat_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let rest = crate::transport::Rest::production(&token);
    rest.send_markup(&chat, &crate::render::markdown_to_html(HELLO), &Value::Null)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The whole wizard. Returns `true` when a config was written.
pub async fn run(
    config_path: &Path,
    io: &mut dyn Prompter,
    path: SetupPath,
) -> Result<bool, String> {
    run_with(
        config_path,
        io,
        path,
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
    path: SetupPath,
    wiring: &Wiring<'_>,
) -> Result<bool, String> {
    match path {
        SetupPath::Ask(_) | SetupPath::Create => {
            if !io.is_terminal() {
                return Err("setup needs a terminal".to_string());
            }
        }
        SetupPath::TokenStdin(_) => {}
    }
    if config_path.exists() && !io.confirm("A config exists here; overwrite it? [y/N] ")? {
        return Ok(false);
    }

    let mut phase = match path {
        SetupPath::Ask(owner) => Phase::AskPath(owner),
        SetupPath::Create => Phase::Create,
        SetupPath::TokenStdin(owner) => Phase::Token {
            owner,
            source: TokenSource::Stdin,
        },
    };
    let base = setup_base();
    let identity = loop {
        phase = match phase {
            Phase::AskPath(owner) => ask_path(io, owner)?,
            Phase::Create => match create_bot(io, wiring, &base).await? {
                Creation::Bot(identity) => Phase::Configure(identity),
                Creation::Fallback => Phase::Token {
                    owner: OwnerSource::UserIds,
                    source: TokenSource::Interactive,
                },
            },
            Phase::Token { owner, source } => {
                Phase::Configure(token_identity(io, wiring, owner, source).await?)
            }
            Phase::Configure(identity) => break identity,
        };
    };
    configure(config_path, io, identity, wiring).await
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
