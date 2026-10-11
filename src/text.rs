//! Message splitting and HTML safety for Telegram's 4096-char limit.
//! Telegram counts message length in UTF-16 code units, not code points.

/// Count UTF-16 code units in `s` (surrogate pairs count as two).
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Split `s` into chunks of at most `limit` UTF-16 units, never slicing a
/// character in half. Returns every character exactly once, in order.
pub fn split_message(s: &str, limit: usize) -> Result<Vec<String>, String> {
    if limit < 2 {
        return Err("Message limit must be at least two UTF-16 units".to_string());
    }
    let mut chunks = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let prefix = prefix_within_limit(rest, limit);
        debug_assert!(!prefix.is_empty(), "limit >= 2 always fits one char");
        chunks.push(prefix.to_string());
        rest = &rest[prefix.len()..];
    }
    Ok(chunks)
}

/// Split markdown at line boundaries into `limit`-UTF-16-unit pages;
/// ``` fences are closed at a page end and reopened on the next.
pub fn paginate_markdown(text: &str, limit: usize) -> Vec<String> {
    let mut pages: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut fence: Option<String> = None;
    let close = |pages: &mut Vec<String>, cur: &mut String, fence: &Option<String>| {
        let mut page = std::mem::take(cur);
        if let Some(lang) = fence {
            page.push_str("\n```");
            *cur = format!("```{lang}");
        }
        pages.push(page);
    };
    for line in text.split('\n') {
        if utf16_len(line) > limit {
            if !cur.is_empty() {
                close(&mut pages, &mut cur, &fence);
            }
            let mut rest = line;
            while utf16_len(rest) > limit {
                let cut = rest
                    .char_indices()
                    .scan(0usize, |used, (i, c)| {
                        *used += c.len_utf16();
                        Some((i + c.len_utf8(), *used))
                    })
                    .take_while(|(_, w)| *w <= limit)
                    .last()
                    .map(|(end, _)| end)
                    .unwrap_or(0);
                match &fence {
                    Some(lang) => pages.push(format!("```{lang}\n{}\n```", &rest[..cut])),
                    None => pages.push(rest[..cut].to_string()),
                }
                rest = &rest[cut..];
            }
            cur = match &fence {
                Some(lang) => format!("```{lang}\n{rest}"),
                None => rest.to_string(),
            };
        } else {
            let join = if cur.is_empty() { 0 } else { 1 };
            if !cur.is_empty() && utf16_len(&cur) + join + utf16_len(line) > limit {
                close(&mut pages, &mut cur, &fence);
            }
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(line);
        }
        let t = line.trim_start();
        if let Some(marker) = t.strip_prefix("```") {
            fence = if fence.is_none() {
                Some(marker.trim().to_string())
            } else {
                None
            };
        }
    }
    if !cur.is_empty() || pages.is_empty() {
        pages.push(cur);
    }
    pages
}

/// Longest prefix of `s` (on a char boundary) within `limit` UTF-16 units.
pub fn prefix_within_limit(s: &str, limit: usize) -> &str {
    if utf16_len(s) <= limit {
        return s;
    }
    let mut used = 0usize;
    let mut end = 0usize;
    for (i, c) in s.char_indices() {
        let w = c.len_utf16();
        if used + w > limit {
            break;
        }
        used += w;
        end = i + c.len_utf8();
    }
    &s[..end]
}

/// Escape `&`, `<`, `>` for a parse_mode=HTML payload. Used by callers that
/// build markup around untrusted text (pairing replies, filenames).
pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Strip characters Telegram never renders (NUL and control codes other
/// than whitespace); a stray one would only ever arrive from odd model
/// output or a pasted file.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t' | '\r'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_respects_utf16() {
        let s = "a".repeat(10);
        let chunks = split_message(&s, 4).unwrap();
        assert_eq!(chunks, vec!["aaaa", "aaaa", "aa"]);
    }

    #[test]
    fn split_never_halves_a_surrogate_pair() {
        let s = format!("{}\u{1f525}{}", "x".repeat(3), "y".repeat(3));
        let chunks = split_message(&s, 4).unwrap();
        assert_eq!(chunks, vec!["xxx", "\u{1f525}yy", "y"]);
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn sanitize_drops_nul_keeps_newline() {
        assert_eq!(sanitize("a\0b\nc"), "ab\nc");
    }
}
