//! Owner/allowlist admission + one-shot pairing.
use std::hint::black_box;

/// Admit one inbound message. Returns the cleaned prompt, or `None` to drop:
/// unknown/empty owner, bot authors, non-allowlisted senders, group messages
/// that neither mention the bot nor reply to it, and empty text.
#[allow(clippy::too_many_arguments)]
pub fn incoming(
    author: &str,
    owner: &str,
    bot: bool,
    private_chat: bool,
    text: &str,
    bot_username: &str,
    mentioned: bool,
    reply_to_bot: bool,
    allowed: &[String],
) -> Option<String> {
    if owner.is_empty() || bot {
        return None;
    }
    if !is_allowed_user(author, owner, allowed) {
        return None;
    }
    let mut cleaned = text.to_string();
    if !private_chat {
        // Groups stay quiet until spoken to directly: an allowed user still
        // needs an @bot mention or a reply to the bot. Scope note: group
        // support beyond the allowlist is out of scope.
        if !mentioned && !reply_to_bot {
            return None;
        }
        let tag = format!("@{bot_username}");
        cleaned = cleaned.replace(&tag, "");
    }
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned)
}

/// Check if a sender is authorized (owner or in allowed_users).
pub fn is_allowed_user(author: &str, owner: &str, allowed: &[String]) -> bool {
    if owner.is_empty() || author.is_empty() {
        return false;
    }
    author == owner || allowed.iter().any(|id| id == author)
}

/// Does this message carry the pairing code? Trim-tolerant, constant-time
/// comparison on the exact code (a contains-match would let any text
/// smuggle the code past the wizard).
pub fn code_matches(text: &str, code: &str) -> bool {
    constant_eq(text.trim(), code)
}

fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= black_box(a[i] ^ b[i]);
    }
    black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_dm_is_admitted() {
        assert_eq!(
            incoming("1", "1", false, true, "hi", "bot", false, false, &[]),
            Some("hi".into())
        );
    }

    #[test]
    fn stranger_is_dropped() {
        assert_eq!(
            incoming("2", "1", false, true, "hi", "bot", false, false, &[]),
            None
        );
    }

    #[test]
    fn group_needs_a_mention() {
        assert_eq!(
            incoming("1", "1", false, false, "hi", "bot", false, false, &[]),
            None
        );
        assert_eq!(
            incoming(
                "1",
                "1",
                false,
                false,
                "hey @bot hi",
                "bot",
                true,
                false,
                &[]
            ),
            Some("hey  hi".into())
        );
    }

    #[test]
    fn code_match_is_exact() {
        assert!(code_matches(" ABCDEF12 ", "ABCDEF12"));
        assert!(!code_matches("xABCDEF12", "ABCDEF12"));
    }
}
