//! Bot commands arrive as ordinary messages whose text starts with `/`.
//! Telegram has no registration step the way Discord slash commands do —
//! `/start` is what a client sends the first time it opens the chat — so
//! admission and dispatch stay inside the gateway's message path.
//!
//! A command may be suffixed with the bot's own username (`/new@mybot`),
//! which groups add automatically; the suffix is stripped before matching.
//!
//! Past the five built-ins, names resolve against the `gray cmd` registry
//! (a `CommandList` snapshot the gateway caches); a registry that is absent
//! or does not know the name leaves the message to fall through as prompt
//! text, exactly as an unknown `/foo` always has.

/// One parsed command message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    /// `/start` — binds the chat (pairing) or greets an admitted user.
    Start,
    /// `/new` or `/reset` — forget the conversation's session.
    New,
    /// `/stop` — cancel the running turn.
    Stop,
    Steer {
        text: String,
    },
    /// `/status` — queue depth and gateway state.
    Status,
    /// `/help` — what the bot can do.
    Help,
    /// A `gray cmd`-registered command (`/model`, `/usage`, …): the name as
    /// the user typed it (aliases resolve server-side) and the raw args.
    Gray {
        name: String,
        args: String,
    },
}

/// Parse `text` as a command. `bot_username` is lowercased without `@`.
/// A command addressed to a different bot (`/new@someone_else`) is not ours.
/// `registry` is the cached `gray cmd list` snapshot — `None` on old
/// binaries, which is how every unknown name stays a plain prompt.
pub fn parse(
    text: &str,
    bot_username: &str,
    registry: Option<&crate::graycmd::CommandList>,
) -> Option<BotCommand> {
    let head = text.trim_start();
    let first = head.split_whitespace().next()?;
    let body = first.strip_prefix('/')?;
    let (name, target) = match body.split_once('@') {
        Some((name, target)) => (name, Some(target)),
        None => (body, None),
    };
    if let Some(target) = target {
        if !target.eq_ignore_ascii_case(bot_username) {
            return None;
        }
    }
    match name.to_ascii_lowercase().as_str() {
        "start" => Some(BotCommand::Start),
        "new" | "reset" => Some(BotCommand::New),
        "stop" => Some(BotCommand::Stop),
        "steer" => Some(BotCommand::Steer {
            text: head[first.len()..].trim().to_string(),
        }),
        "status" => Some(BotCommand::Status),
        "help" => Some(BotCommand::Help),
        _ => registry
            .and_then(|list| list.lookup(name))
            .map(|_| BotCommand::Gray {
                name: name.to_string(),
                args: head[first.len()..].trim().to_string(),
            }),
    }
}

pub const HELP: &str = "I relay messages to a local gray agent.\n\n\
     /new — start a fresh conversation\n\
     /stop — cancel the running task\n\
     /steer <text> — guide the running task (idle: sent as a new message)\n\
     /status — what the queue is doing\n\
     /help — this text\n\n\
     Attach a photo or document and I will hand it to the agent.";

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> crate::graycmd::CommandList {
        crate::graycmd::CommandList::from_reply(&serde_json::json!({
            "ok": true, "protocol": 1,
            "result": {"commands": [
                {"name": "model", "description": "pick the model",
                 "aliases": ["m"], "args": [],
                 "scope": "conversation", "surface": "interactive"},
                {"name": "usage", "description": "tokens", "aliases": [],
                 "args": [], "scope": "conversation", "surface": "text"}
            ]}
        }))
        .unwrap()
    }

    #[test]
    fn parses_with_and_without_bot_suffix() {
        assert_eq!(parse("/start", "bot", None), Some(BotCommand::Start));
        assert_eq!(parse("/new@bot", "bot", None), Some(BotCommand::New));
        assert_eq!(parse("/new@other", "bot", None), None);
        assert_eq!(parse("hello", "bot", None), None);
        assert_eq!(parse("/unknown", "bot", None), None);
    }

    #[test]
    fn unknown_stays_unknown_without_registry() {
        // The zero-regression path: no snapshot, `/model` is not a command.
        assert_eq!(parse("/model", "bot", None), None);
    }

    #[test]
    fn registry_names_and_aliases_become_gray() {
        let list = registry();
        assert_eq!(
            parse("/model", "bot", Some(&list)),
            Some(BotCommand::Gray {
                name: "model".to_string(),
                args: String::new()
            })
        );
        assert_eq!(
            parse("/m gpt-5 high", "bot", Some(&list)),
            Some(BotCommand::Gray {
                name: "m".to_string(),
                args: "gpt-5 high".to_string()
            })
        );
        assert_eq!(
            parse("/model@bot x", "bot", Some(&list)),
            Some(BotCommand::Gray {
                name: "model".to_string(),
                args: "x".to_string()
            })
        );
        // Built-ins still win even when the registry knows the name too.
        assert_eq!(parse("/help", "bot", Some(&list)), Some(BotCommand::Help));
        // Registered for another bot — not ours.
        assert_eq!(parse("/model@other", "bot", Some(&list)), None);
        // Registry is present but does not know this name.
        assert_eq!(parse("/bogus", "bot", Some(&list)), None);
    }
}
