use serde_json::Value;

const MAX_PREVIEW_CHARS: usize = 80;
const MAX_COMMAND_CHARS: usize = 240;

pub fn enabled(config: &Value) -> bool {
    config
        .get("activity_indicator")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !crate::stream::quiet(config))
}

fn label(tool: &str, name: &str) -> String {
    match tool {
        "bash" | "shell" => "Running".to_string(),
        "read" | "cat" => "Read".to_string(),
        "write" | "create" | "str_replace" => "Write".to_string(),
        "edit" | "apply_patch" => "Edit".to_string(),
        "" => "Working".to_string(),
        _ => name.to_string(),
    }
}

fn name_of(row: &Value) -> String {
    row.get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| humanize(row.get("tool").and_then(Value::as_str).unwrap_or("")))
}

pub fn humanize(name: &str) -> String {
    if !name.contains(['_', '-']) {
        return name.to_string();
    }
    name.split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn line(row: &Value, elapsed: Option<u64>) -> Option<String> {
    let phase = row.get("phase").and_then(Value::as_str).unwrap_or("");
    let tool = row.get("tool").and_then(Value::as_str).unwrap_or("");
    let detail = row.get("detail").and_then(Value::as_str).unwrap_or("");
    let name = name_of(row);
    match phase {
        "tool_started" => None,
        "tool_ran" => Some(shell_line(tool, &name, detail, elapsed, elapsed.is_some())),
        "tool_finished" if row.get("error").and_then(Value::as_bool) == Some(true) => Some(
            format!("{} failed", if tool.is_empty() { "Tool" } else { &name }),
        ),
        "provider_retry" if !detail.is_empty() => {
            Some(format!("Provider retry: {}", one_line(detail)))
        }
        "compacted" => Some("Context compacted".to_string()),
        _ => None,
    }
}

fn shell_line(tool: &str, name: &str, detail: &str, elapsed: Option<u64>, done: bool) -> String {
    let arg = match tool {
        "bash" | "shell" => clean_command(detail),
        "" => String::new(),
        _ => one_line(detail),
    };
    let mut out = match tool {
        "bash" | "shell" if arg.is_empty() && done => "Checked background job".to_string(),
        "bash" | "shell" if arg.is_empty() => "Checking background job".to_string(),
        "bash" | "shell" if done => "Ran".to_string(),
        _ => label(tool, name),
    };
    if !arg.is_empty() {
        out.push(' ');
        out.push_str(&preview(&arg));
    }
    match elapsed {
        Some(ms) => format!("{out} ({})", secs(ms)),
        None => out,
    }
}

fn preview(text: &str) -> String {
    format!("`{}`", capped(text, MAX_PREVIEW_CHARS))
}

fn secs(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

fn clean_command(detail: &str) -> String {
    let first = detail
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    capped(&one_line(first), MAX_COMMAND_CHARS)
}

fn capped(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn elapsed_by_call(rows: &[Value]) -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
    for row in rows {
        if row.get("phase").and_then(Value::as_str) != Some("tool_finished") {
            continue;
        }
        if let (Some(id), Some(ms)) = (
            row.get("call_id").and_then(Value::as_str),
            row.get("elapsed_ms").and_then(Value::as_u64),
        ) {
            map.insert(id.to_string(), ms);
        }
    }
    map
}

fn elapsed_of(row: &Value, by_call: &std::collections::HashMap<String, u64>) -> Option<u64> {
    row.get("call_id")
        .and_then(Value::as_str)
        .and_then(|id| by_call.get(id).copied())
}

fn feed_lines(rows: &[Value]) -> Vec<String> {
    let by_call = elapsed_by_call(rows);
    let mut lines: Vec<String> = Vec::new();
    for row in rows {
        let Some(text) = line(row, elapsed_of(row, &by_call)) else {
            continue;
        };
        if lines.last() == Some(&text) {
            continue;
        }
        lines.push(text);
    }
    lines
}

pub fn feed(rows: &[Value], budget: usize) -> String {
    let lines = feed_lines(rows);
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev() {
        let cost = line.chars().count() + 1;
        if used + cost > budget && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(line.as_str());
    }
    kept.reverse();
    kept.join("\n")
}

pub fn summary(rows: &[Value]) -> String {
    counts(
        rows.iter()
            .filter(|row| row.get("phase").and_then(Value::as_str) == Some("tool_ran"))
            .map(|row| row.get("tool").and_then(Value::as_str).unwrap_or("")),
    )
    .join(" · ")
}

fn counts<'a>(tools: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut ran = 0usize;
    let mut read = 0usize;
    let mut edited = 0usize;
    let mut other = 0usize;
    for tool in tools {
        match tool {
            "bash" | "shell" => ran += 1,
            "read" | "cat" | "find" | "grep" | "glob" | "ls" => read += 1,
            "write" | "create" | "str_replace" | "edit" | "apply_patch" => edited += 1,
            "" => {}
            _ => other += 1,
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if edited > 0 {
        parts.push(format!("edited {edited} file{}", plural(edited)));
    }
    if read > 0 {
        parts.push(format!("read {read} file{}", plural(read)));
    }
    if ran > 0 {
        parts.push(format!("ran {ran} command{}", plural(ran)));
    }
    if other > 0 {
        parts.push(format!("called {other} tool{}", plural(other)));
    }
    parts
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
