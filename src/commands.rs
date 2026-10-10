//! Bot commands arrive as ordinary messages whose text starts with `/`.
//! Telegram has no registration step the way Discord slash commands do —
//! `/start` is what a client sends the first time it opens the chat — so
//! admission and dispatch stay inside the gateway's message path.
//!
//! A command may be suffixed with the bot's own username (`/new@mybot`),
//! which groups add automatically; the suffix is stripped before matching.

/// One parsed command message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    /// `/start` — binds the chat (pairing) or greets an admitted user.
    Start,
    /// `/new` or `/reset` — forget the conversation's session.
    New,
    /// `/stop` — cancel the running turn.
    Stop,
    /// `/status` — queue depth and gateway state.
    Status,
    /// `/help` — what the bot can do.
    Help,
}

/// Parse `text` as a command. `bot_username` is lowercased without `@`.
/// A command addressed to a different bot (`/new@someone_else`) is not ours.
pub fn parse(text: &str, bot_username: &str) -> Option<BotCommand> {
    let first = text.trim_start().split_whitespace().next()?;
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
        "status" => Some(BotCommand::Status),
        "help" => Some(BotCommand::Help),
        _ => None,
    }
}

pub const HELP: &str = "I relay messages to a local gray agent.\n\n\
     /new — start a fresh conversation\n\
     /stop — cancel the running task\n\
     /status — what the queue is doing\n\
     /help — this text\n\n\
     Attach a photo or document and I will hand it to the agent.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_and_without_bot_suffix() {
        assert_eq!(parse("/start", "bot"), Some(BotCommand::Start));
        assert_eq!(parse("/new@bot", "bot"), Some(BotCommand::New));
        assert_eq!(parse("/new@other", "bot"), None);
        assert_eq!(parse("hello", "bot"), None);
        assert_eq!(parse("/unknown", "bot"), None);
    }
}
