use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

const GAP: &str = "\n\n";
const RULE: &str = "────────────────────────";
const QUOTE_LINES: usize = 8;
const TAG_SCAN: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Face {
    Html,
    Plain,
}

impl Face {
    fn html(self) -> bool {
        matches!(self, Face::Html)
    }
}

pub fn markdown_to_html(md: &str) -> String {
    render(md, Face::Html).join(GAP)
}

pub fn markdown_to_text(md: &str) -> String {
    render(md, Face::Plain).join(GAP)
}

pub fn split_markdown(md: &str, limit: usize) -> Vec<String> {
    pack(render(md, Face::Html), limit.max(64))
}

pub fn caption_html(md: &str, limit: usize) -> String {
    let mut chunks = pack(render(md, Face::Html), limit.max(64).saturating_sub(8));
    if chunks.len() <= 1 {
        return chunks.pop().unwrap_or_default();
    }
    format!("{} …", chunks.remove(0))
}

pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&unescape(&rest[..start]));
        let tail = &rest[start..];
        let Some(end) = tail.find('>') else {
            out.push_str(&unescape(tail));
            return out;
        };
        if breaks_block(&tail[..=end]) {
            out.push('\n');
        }
        rest = &tail[end + 1..];
    }
    out.push_str(&unescape(rest));
    out
}

fn breaks_block(tag: &str) -> bool {
    let name = tag_name(tag);
    (tag.starts_with("</") && matches!(name, "pre" | "blockquote" | "p")) || matches!(name, "br")
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let mut consumed = 0;
        for (offset, ch) in tail.char_indices().skip(1) {
            if ch == ';' {
                consumed = offset + 1;
                break;
            }
            if !(ch.is_ascii_alphanumeric() || ch == '#') || offset > TAG_SCAN {
                break;
            }
        }
        if consumed == 0 {
            out.push('&');
            rest = &tail[1..];
            continue;
        }
        let entity = &tail[1..consumed - 1];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            "nbsp" => out.push(' '),
            _ => out.push_str(&tail[..consumed]),
        }
        rest = &tail[consumed..];
    }
    out.push_str(rest);
    out
}

fn pack(blocks: Vec<String>, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for block in blocks {
        let pieces = if crate::text::utf16_len(&block) <= limit {
            vec![block]
        } else {
            split_html(&block, limit)
        };
        for piece in pieces {
            if piece.is_empty() {
                continue;
            }
            let used = crate::text::utf16_len(&current);
            if !current.is_empty() && used + 2 + crate::text::utf16_len(&piece) <= limit {
                current.push_str(GAP);
                current.push_str(&piece);
            } else {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                current = piece;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn split_html(html: &str, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut open: Vec<(String, String)> = Vec::new();
    let mut used = 0usize;
    let mut at = 0usize;
    while at < html.len() {
        let (token, next) = next_token(html, at);
        let width = crate::text::utf16_len(token);
        let closing: usize = open
            .iter()
            .map(|(_, close)| crate::text::utf16_len(close))
            .sum();
        if used > 0 && used + width + closing > limit {
            let mut piece = std::mem::take(&mut current);
            for (_, close) in open.iter().rev() {
                piece.push_str(close);
            }
            out.push(piece);
            used = 0;
            for (opening, _) in &open {
                current.push_str(opening);
                used += crate::text::utf16_len(opening);
            }
        }
        current.push_str(token);
        used += width;
        if let Some(name) = token
            .strip_prefix("</")
            .and_then(|rest| rest.strip_suffix('>'))
        {
            if let Some(pos) = open
                .iter()
                .rposition(|(opening, _)| tag_name(opening) == name)
            {
                open.truncate(pos);
            }
        } else if token.starts_with('<') && !token.ends_with("/>") {
            let name = tag_name(token);
            open.push((token.to_string(), format!("</{name}>")));
        }
        at = next;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn next_token(text: &str, at: usize) -> (&str, usize) {
    let rest = &text[at..];
    if rest.starts_with('<') {
        if let Some(end) = rest.find('>') {
            return (&rest[..=end], at + end + 1);
        }
    }
    if rest.starts_with('&') {
        for (offset, ch) in rest.char_indices().skip(1) {
            if ch == ';' {
                return (&rest[..=offset], at + offset + 1);
            }
            if !(ch.is_ascii_alphanumeric() || ch == '#') || offset > TAG_SCAN {
                break;
            }
        }
    }
    let len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
    (&rest[..len], at + len)
}

fn tag_name(tag: &str) -> &str {
    let stripped = tag.trim_start_matches('<').trim_start_matches('/');
    let end = stripped
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(stripped.len());
    &stripped[..end]
}

fn escape_attr(text: &str) -> String {
    crate::text::escape_html(text).replace('"', "&quot;")
}

fn render(md: &str, face: Face) -> Vec<String> {
    let cleaned = crate::text::sanitize(md).replace('\r', "");
    let mut walker = Walker::new(&cleaned, face);
    walker.blocks(None)
}

struct Walker<'a> {
    events: Vec<Event<'a>>,
    at: usize,
    face: Face,
    depth: usize,
    quote: usize,
}

impl<'a> Walker<'a> {
    fn new(md: &'a str, face: Face) -> Self {
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        Self {
            events: Parser::new_ext(md, options).collect(),
            at: 0,
            face,
            depth: 0,
            quote: 0,
        }
    }

    fn peek(&self) -> Option<Event<'a>> {
        self.events.get(self.at).cloned()
    }

    fn blocks(&mut self, end: Option<TagEnd>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut pending = String::new();
        while let Some(event) = self.peek() {
            if let Event::End(tag) = &event {
                let tag = *tag;
                self.at += 1;
                if Some(tag) == end {
                    break;
                }
                continue;
            }
            match event {
                Event::Start(Tag::Paragraph) => {
                    flush(&mut out, &mut pending);
                    self.at += 1;
                    let text = self.inline_until(TagEnd::Paragraph);
                    let text = text.trim_end().to_string();
                    if !text.trim().is_empty() {
                        out.push(text);
                    }
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    flush(&mut out, &mut pending);
                    self.at += 1;
                    let inner = self.inline_until(TagEnd::Heading(level));
                    let block = self.heading(&inner);
                    if !block.is_empty() {
                        out.push(block);
                    }
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    flush(&mut out, &mut pending);
                    self.at += 1;
                    let block = self.code_block(&kind);
                    if !block.trim().is_empty() {
                        out.push(block);
                    }
                }
                Event::Start(Tag::BlockQuote(kind)) => {
                    flush(&mut out, &mut pending);
                    self.at += 1;
                    self.quote += 1;
                    let inner = self.blocks(Some(TagEnd::BlockQuote(kind)));
                    let block = self.quote_block(&inner);
                    self.quote -= 1;
                    if let Some(block) = block {
                        out.push(block);
                    }
                }
                Event::Start(Tag::List(start)) => {
                    flush(&mut out, &mut pending);
                    self.at += 1;
                    let lines = self.list(start.is_some(), start);
                    if !lines.is_empty() {
                        out.push(lines.join("\n"));
                    }
                }
                Event::Start(Tag::Table(_)) => {
                    flush(&mut out, &mut pending);
                    self.at += 1;
                    let block = self.table();
                    if !block.trim().is_empty() {
                        out.push(block);
                    }
                }
                Event::Start(Tag::HtmlBlock) => {
                    flush(&mut out, &mut pending);
                    self.at += 1;
                    let raw = self.raw_until(TagEnd::HtmlBlock);
                    let raw = raw.trim_end().to_string();
                    if !raw.trim().is_empty() {
                        out.push(self.escape(&raw));
                    }
                }
                Event::Start(Tag::FootnoteDefinition(_)) => {
                    self.at += 1;
                    let _ = self.blocks(Some(TagEnd::FootnoteDefinition));
                }
                Event::Start(Tag::Item) => {
                    self.at += 1;
                    let _ = self.blocks(Some(TagEnd::Item));
                }
                other => pending.push_str(&self.inline_event(other)),
            }
        }
        flush(&mut out, &mut pending);
        out
    }

    fn inline_until(&mut self, end: TagEnd) -> String {
        let mut out = String::new();
        while let Some(event) = self.peek() {
            if event == Event::End(end) {
                self.at += 1;
                break;
            }
            out.push_str(&self.inline_event(event));
        }
        out
    }

    fn inline_event(&mut self, event: Event<'a>) -> String {
        let mut out = String::new();
        match event {
            Event::Start(Tag::Emphasis) => {
                self.at += 1;
                out.push_str(&self.wrap(TagEnd::Emphasis, "<i>", "</i>"));
            }
            Event::Start(Tag::Strong) => {
                self.at += 1;
                out.push_str(&self.wrap(TagEnd::Strong, "<b>", "</b>"));
            }
            Event::Start(Tag::Strikethrough) => {
                self.at += 1;
                out.push_str(&self.wrap(TagEnd::Strikethrough, "<s>", "</s>"));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                self.at += 1;
                let inner = self.inline_until(TagEnd::Heading(level));
                out.push_str(&self.heading(&inner));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.at += 1;
                out.push_str(&self.code_block(&kind));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.at += 1;
                let inner = self.inline_until(TagEnd::Link);
                out.push_str(&self.link(&dest_url, &inner));
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                self.at += 1;
                let alt = self.inline_until(TagEnd::Image);
                out.push_str(&self.link(&dest_url, &alt));
            }
            Event::Start(Tag::Paragraph) => {
                self.at += 1;
                out.push_str(&self.inline_until(TagEnd::Paragraph));
                out.push('\n');
            }
            Event::Start(Tag::BlockQuote(kind)) => {
                self.at += 1;
                self.quote += 1;
                let inner = self.blocks(Some(TagEnd::BlockQuote(kind)));
                let block = self.quote_block(&inner);
                self.quote -= 1;
                if let Some(block) = block {
                    out.push_str(&block);
                }
            }
            Event::Start(Tag::List(start)) => {
                self.at += 1;
                out.push_str(&self.list(start.is_some(), start).join("\n"));
            }
            Event::Start(_) => self.at += 1,
            Event::End(_) => self.at += 1,
            Event::Text(text) => {
                self.at += 1;
                out.push_str(&self.escape(&text));
            }
            Event::Code(code) => {
                self.at += 1;
                out.push_str(&match self.face {
                    Face::Html => format!("<code>{}</code>", crate::text::escape_html(&code)),
                    Face::Plain => code.to_string(),
                });
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                self.at += 1;
                out.push_str(&self.escape(&math));
            }
            Event::Html(raw) | Event::InlineHtml(raw) => {
                self.at += 1;
                out.push_str(&self.escape(&raw));
            }
            Event::FootnoteReference(_) => self.at += 1,
            Event::SoftBreak | Event::HardBreak => {
                self.at += 1;
                out.push('\n');
            }
            Event::Rule => {
                self.at += 1;
                out.push_str(RULE);
            }
            Event::TaskListMarker(checked) => {
                self.at += 1;
                out.push_str(if checked { "☑ " } else { "☐ " });
            }
        }
        out
    }

    fn raw_until(&mut self, end: TagEnd) -> String {
        let mut out = String::new();
        while let Some(event) = self.peek() {
            if event == Event::End(end) {
                self.at += 1;
                break;
            }
            match event {
                Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                    self.at += 1;
                    out.push_str(&text);
                }
                Event::SoftBreak | Event::HardBreak => {
                    self.at += 1;
                    out.push('\n');
                }
                _ => self.at += 1,
            }
        }
        out
    }

    fn wrap(&mut self, end: TagEnd, open: &str, close: &str) -> String {
        let inner = self.inline_until(end);
        if inner.trim().is_empty() {
            return inner;
        }
        match self.face {
            Face::Html => format!("{open}{inner}{close}"),
            Face::Plain => inner,
        }
    }

    fn escape(&self, text: &str) -> String {
        match self.face {
            Face::Html => crate::text::escape_html(text),
            Face::Plain => text.to_string(),
        }
    }

    fn heading(&self, inner: &str) -> String {
        let text = if self.face.html() {
            inner.replace("<b>", "").replace("</b>", "")
        } else {
            inner.to_string()
        };
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }
        match self.face {
            Face::Html => format!("<b>{text}</b>"),
            Face::Plain => text.to_string(),
        }
    }

    fn link(&self, dest: &str, inner: &str) -> String {
        let label = inner.trim();
        match self.face {
            Face::Html => {
                let label = if label.is_empty() {
                    crate::text::escape_html(dest)
                } else {
                    inner.to_string()
                };
                format!("<a href=\"{}\">{label}</a>", escape_attr(dest))
            }
            Face::Plain if label.is_empty() || label == dest.trim() => dest.to_string(),
            Face::Plain => format!("{inner} ({dest})"),
        }
    }

    fn quote_block(&self, blocks: &[String]) -> Option<String> {
        let joined = blocks.join("\n");
        if joined.trim().is_empty() {
            return None;
        }
        if !self.face.html() || self.quote > 1 {
            return Some(joined);
        }
        if joined.lines().count() > QUOTE_LINES {
            Some(format!("<blockquote expandable>{joined}</blockquote>"))
        } else {
            Some(format!("<blockquote>{joined}</blockquote>"))
        }
    }

    fn code_block(&mut self, kind: &CodeBlockKind) -> String {
        let lang = match kind {
            CodeBlockKind::Fenced(info) => language(info),
            CodeBlockKind::Indented => String::new(),
        };
        let mut body = String::new();
        while let Some(event) = self.peek() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    self.at += 1;
                    break;
                }
                Event::Text(text) => {
                    self.at += 1;
                    body.push_str(&text);
                }
                Event::SoftBreak | Event::HardBreak => {
                    self.at += 1;
                    body.push('\n');
                }
                _ => self.at += 1,
            }
        }
        let body = body.trim_end_matches('\n');
        if !self.face.html() {
            return body.to_string();
        }
        let text = crate::text::escape_html(body);
        if lang.is_empty() {
            format!("<pre><code>{text}</code></pre>")
        } else {
            format!("<pre><code class=\"language-{lang}\">{text}</code></pre>")
        }
    }

    fn list(&mut self, ordered: bool, start: Option<u64>) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut number = start.unwrap_or(1);
        while let Some(event) = self.peek() {
            match event {
                Event::Start(Tag::Item) => {
                    self.at += 1;
                    let depth = self.depth;
                    lines.extend(self.item(ordered, number));
                    self.depth = depth;
                    if ordered {
                        number += 1;
                    }
                }
                Event::End(TagEnd::List(_)) => {
                    self.at += 1;
                    break;
                }
                _ => self.at += 1,
            }
        }
        lines
    }

    fn item(&mut self, ordered: bool, number: u64) -> Vec<String> {
        let depth = self.depth;
        let indent = "  ".repeat(depth);
        let mut mark = if ordered {
            format!("{number}. ")
        } else {
            "• ".to_string()
        };
        let mut out: Vec<String> = Vec::new();
        let mut pending = String::new();
        let mut first = true;
        while let Some(event) = self.peek() {
            match event {
                Event::End(TagEnd::Item) => {
                    self.at += 1;
                    break;
                }
                Event::TaskListMarker(checked) => {
                    self.at += 1;
                    if first {
                        mark = if checked {
                            "☑ ".to_string()
                        } else {
                            "☐ ".to_string()
                        };
                    }
                }
                Event::Start(Tag::Paragraph) => {
                    self.at += 1;
                    pending.push_str(&self.inline_until(TagEnd::Paragraph));
                }
                Event::Start(Tag::List(start)) => {
                    self.at += 1;
                    item_line(&mut out, &mut pending, &mut first, &indent, &mark);
                    self.depth = depth + 1;
                    let sub = self.list(start.is_some(), start);
                    self.depth = depth;
                    out.extend(sub);
                    first = false;
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    self.at += 1;
                    item_line(&mut out, &mut pending, &mut first, &indent, &mark);
                    let block = self.code_block(&kind);
                    for line in block.lines() {
                        out.push(format!("{indent}  {line}"));
                    }
                    first = false;
                }
                other => pending.push_str(&self.inline_event(other)),
            }
        }
        item_line(&mut out, &mut pending, &mut first, &indent, &mark);
        if out.is_empty() {
            out.push(format!("{indent}{}", mark.trim_end()));
        }
        out
    }

    fn table(&mut self) -> String {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut head_rows = 0usize;
        while let Some(event) = self.peek() {
            match event {
                Event::Start(Tag::TableHead) => {
                    self.at += 1;
                    rows.push(Vec::new());
                }
                Event::End(TagEnd::TableHead) => {
                    self.at += 1;
                    head_rows += 1;
                }
                Event::Start(Tag::TableRow) => {
                    self.at += 1;
                    rows.push(Vec::new());
                }
                Event::Start(Tag::TableCell) => {
                    self.at += 1;
                    let cell = self.inline_until(TagEnd::TableCell);
                    let cell = html_to_text(&cell);
                    let cell = cell
                        .split('\n')
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if let Some(row) = rows.last_mut() {
                        row.push(cell.trim().to_string());
                    }
                }
                Event::End(TagEnd::Table) => {
                    self.at += 1;
                    break;
                }
                _ => self.at += 1,
            }
        }
        rows.retain(|row| !row.is_empty());
        render_table(&rows, head_rows, self.face)
    }
}

fn item_line(
    out: &mut Vec<String>,
    pending: &mut String,
    first: &mut bool,
    indent: &str,
    mark: &str,
) {
    let text = pending.trim().to_string();
    pending.clear();
    if text.is_empty() {
        return;
    }
    if *first {
        out.push(format!("{indent}{mark}{text}"));
        *first = false;
    } else {
        out.push(format!("{indent}  {text}"));
    }
}

fn flush(out: &mut Vec<String>, pending: &mut String) {
    let text = pending.trim_end().to_string();
    pending.clear();
    if !text.trim().is_empty() {
        out.push(text);
    }
}

fn language(info: &str) -> String {
    info.split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.'))
        .collect()
}

fn render_table(rows: &[Vec<String>], head_rows: usize, face: Face) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return String::new();
    }
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for (col, width) in widths.iter().enumerate() {
            let cell = row.get(col).map(String::as_str).unwrap_or("");
            if col > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            for _ in cell.chars().count()..*width {
                line.push(' ');
            }
        }
        lines.push(line);
        if index + 1 == head_rows {
            let rule = widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("  ");
            lines.push(rule);
        }
    }
    let body = lines.join("\n");
    match face {
        Face::Html => format!("<pre>{}</pre>", crate::text::escape_html(&body)),
        Face::Plain => body,
    }
}
