//! Restart and shutdown notices: chats with a turn in flight hear that it
//! is about to be cut short, and the home chat hears when the gateway is
//! back, including after a crash, kill, or reboot that gave it no chance to
//! say goodbye.
//!
//! gray core owns this, platform-agnostic: `gray gateway lifecycle
//! boot|stop --dir <config dir>` decides how the last run ended and hands
//! back the wording; this adapter only posts it ([`boot_via`],
//! [`stop_via`]). The local copy below is the fallback for a gray that
//! predates the subcommand, and reads and writes the same files.
//!
//! State is two files next to the config:
//! - `lifecycle.json`: `running` while the daemon is up, `stopped` after a
//!   clean exit. Still `running` at the next boot means the last run died.
//! - `restart_pending`: written by `gray telegram restart` before it
//!   signals the daemon, so a SIGTERM reads as "restarting" instead of
//!   "shutting down". The next boot removes it.
use serde_json::{json, Value};
use std::path::Path;

pub const STATE_FILE: &str = "lifecycle.json";
pub const RESTART_MARKER: &str = "restart_pending";

/// How the previous daemon run ended, read once at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Previous {
    /// No record: first boot, nothing to announce.
    FirstBoot,
    /// Exited on a signal after saying so.
    Clean { restart: bool },
    /// Still marked running: crash, SIGKILL, OOM, or power loss.
    Crashed,
}

/// `restart_notification: false` in the config silences every notice.
pub fn enabled(config: &Value) -> bool {
    config
        .get("restart_notification")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Reads how the last run ended, then records this one as running.
pub fn boot(dir: &Path) -> Previous {
    let restart_marker = dir.join(RESTART_MARKER);
    let restart_requested = restart_marker.exists();
    let _ = std::fs::remove_file(&restart_marker);
    let previous = std::fs::read(dir.join(STATE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let previous = match previous {
        None => Previous::FirstBoot,
        Some(state) => match state.get("state").and_then(Value::as_str) {
            Some("stopped") => Previous::Clean {
                restart: restart_requested
                    || state.get("restart").and_then(Value::as_bool) == Some(true),
            },
            _ => Previous::Crashed,
        },
    };
    write(
        dir,
        json!({"state": "running", "at": crate::durable::now_secs()}),
    );
    previous
}

/// Whether a restart (not a plain stop) was asked for.
pub fn restart_requested(dir: &Path) -> bool {
    dir.join(RESTART_MARKER).exists()
}

/// Called by `gray telegram restart` before the old process is signalled.
pub fn request_restart(dir: &Path) {
    let _ = std::fs::write(dir.join(RESTART_MARKER), b"");
}

/// Called by `gray telegram stop`: a leftover marker from a failed restart
/// must not turn this stop into "restarting".
pub fn clear_restart(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(RESTART_MARKER));
}

/// Records a clean exit; the next boot announces "online", not "crashed".
pub fn mark_stopped(dir: &Path, restart: bool) {
    write(
        dir,
        json!({"state": "stopped", "restart": restart, "at": crate::durable::now_secs()}),
    );
}

fn write(dir: &Path, state: Value) {
    let _ = crate::config::atomic_json(&dir.join(STATE_FILE), &state);
}

/// The home-chat notice for this boot; `interrupted` is how many turns were
/// running when the last run ended.
pub fn startup_notice(previous: Previous, interrupted: usize) -> Option<String> {
    let mut text = match previous {
        Previous::FirstBoot => return None,
        Previous::Clean { restart: true } => "Gateway restarted. gray is back and ready.",
        Previous::Clean { restart: false } => "Gateway online. gray is back and ready.",
        Previous::Crashed => {
            "Gateway is back after an unexpected stop (crash, kill, or reboot). gray is ready."
        }
    }
    .to_string();
    match interrupted {
        0 => {}
        1 => text.push_str("\n1 turn was cut short; send a message in its chat to continue."),
        n => text.push_str(&format!(
            "\n{n} turns were cut short; send a message in their chats to continue."
        )),
    }
    Some(text)
}

/// Sent to each chat whose turn is about to be interrupted.
pub fn active_notice(restart: bool) -> &'static str {
    if restart {
        "Gateway restarting: your current task will be interrupted. Send any message after the restart to pick up where you left off."
    } else {
        "Gateway shutting down: your current task will be interrupted. Once it is back online, send any message to pick up where you left off."
    }
}

/// Sent to the home chat when nothing is running there.
pub fn home_notice(restart: bool) -> &'static str {
    if restart {
        "Gateway restarting. Back in a moment."
    } else {
        "Gateway shutting down."
    }
}

/// What a clean exit should say, as core worded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goodbye {
    pub restart: bool,
    /// For each chat with a running turn.
    pub active: String,
    /// For the home chat when nothing ran there.
    pub home: String,
}

/// Ask gray core (`gray gateway lifecycle ...`); `None` when this gray has
/// no such subcommand or does not answer in time.
async fn core(gray_bin: &Path, args: &[&str], dir: &Path) -> Option<Value> {
    let mut cmd = tokio::process::Command::new(gray_bin);
    cmd.args(["gateway", "lifecycle"])
        .args(args)
        .arg("--dir")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(3), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .filter(Value::is_object)
}

/// Boot: record this run and return the home-chat notice, if any.
pub async fn boot_via(gray_bin: &Path, dir: &Path, interrupted: usize) -> Option<String> {
    let count = interrupted.to_string();
    match core(gray_bin, &["boot", "--interrupted", &count], dir).await {
        Some(answer) => answer
            .get("notice")
            .and_then(Value::as_str)
            .map(str::to_string),
        None => startup_notice(boot(dir), interrupted),
    }
}

/// Clean exit: record it and return what chats should hear.
pub async fn stop_via(gray_bin: &Path, dir: &Path) -> Goodbye {
    if let Some(answer) = core(gray_bin, &["stop"], dir).await {
        let text = |key: &str| answer.get(key).and_then(Value::as_str).map(str::to_string);
        if let (Some(active), Some(home)) = (text("active_notice"), text("home_notice")) {
            return Goodbye {
                restart: answer.get("restart").and_then(Value::as_bool) == Some(true),
                active,
                home,
            };
        }
    }
    let restart = restart_requested(dir);
    mark_stopped(dir, restart);
    Goodbye {
        restart,
        active: active_notice(restart).to_string(),
        home: home_notice(restart).to_string(),
    }
}
