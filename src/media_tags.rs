//! `MEDIA:` tags in agent output. `telegram_send` prose may carry
//! `MEDIA:/path/to.png` lines; they are split out of the text and the named
//! files are uploaded after it (images via sendPhoto, the rest
//! sendDocument). The same extraction runs on turn answers so a file the
//! agent produced mid-turn is delivered, not described.
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Telegram accepts uploads up to 50 MB on sendDocument; keep the discord
/// cap so media behavior matches the reference.
pub const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    pub name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub path: PathBuf,
}

impl Upload {
    pub fn is_photo(&self) -> bool {
        self.media_type.starts_with("image/")
    }
}

/// Optional `media_roots` config: when set, files may only come from those
/// directories. Unset (the default) allows any non-secret file.
pub fn roots_from_config(config: &Value) -> Vec<PathBuf> {
    config
        .get("media_roots")
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|root| resolve(root, Path::new("/")))
                .filter_map(|root| root.canonicalize().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Split `text` into its prose and the deliverable files its `MEDIA:` tags
/// name. Relative paths resolve against `cwd`.
pub fn extract(text: &str, cwd: &Path, roots: &[PathBuf]) -> (String, Vec<PathBuf>) {
    let mut out = String::with_capacity(text.len());
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("MEDIA:") {
        let (before, after) = rest.split_at(at);
        let body = &after["MEDIA:".len()..];
        let trimmed = body.trim_start_matches([' ', '\t']);
        let end = trimmed
            .find(|c: char| c.is_whitespace() || c == '`' || c == '"' || c == '\'')
            .unwrap_or(trimmed.len());
        let end = trimmed[..end].find("MEDIA:").unwrap_or(end);
        let raw = trimmed[..end]
            .trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '*', '_', '!', '?']);
        let consumed = body.len() - trimmed.len() + end;
        match resolve(raw, cwd).and_then(|path| deliverable(&path, roots).ok()) {
            Some(path) => {
                out.push_str(before.trim_end_matches(['*', '_', '`']));
                if !paths.contains(&path) {
                    paths.push(path);
                }
                rest = body[consumed..].trim_start_matches(['*', '_', '`']);
            }
            None => {
                out.push_str(before);
                out.push_str("MEDIA:");
                rest = body;
            }
        }
    }
    out.push_str(rest);
    (tidy(&out), paths)
}

/// Collapse the runs of blank lines a removed tag leaves behind.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0;
    for line in text.split('\n') {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

pub fn resolve(raw: &str, cwd: &Path) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    let path = if let Some(home_relative) = raw.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(home_relative)
    } else {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    };
    Some(path)
}

/// A path may be delivered when it is a regular, bounded file that is not a
/// credential or configuration secret, inside `roots` when any are set.
pub fn deliverable(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|_| format!("{} does not exist", path.display()))?;
    let metadata = std::fs::metadata(&canonical).map_err(|_| "file cannot be read".to_string())?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", canonical.display()));
    }
    if metadata.len() == 0 {
        return Err(format!("{} is empty", canonical.display()));
    }
    if metadata.len() > MAX_UPLOAD_BYTES {
        return Err(format!("{} is larger than 25 MiB", canonical.display()));
    }
    if !roots.is_empty() && !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(format!(
            "{} is outside the configured media_roots",
            canonical.display()
        ));
    }
    if is_sensitive(&canonical) {
        return Err(format!(
            "{} looks like a credential or config file and is never uploaded",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn is_sensitive(path: &Path) -> bool {
    const DIRS: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".password-store",
        "keyrings",
    ];
    // Gray's own state directories hold tokens and sessions; only the
    // per-conversation `work` directory inside them is the agent's output.
    const FILES: &[&str] = &[
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        "credentials",
        "credentials.json",
        "config.json",
        "auth.json",
        "token",
        "tokens.json",
        "secrets.json",
        "queue.sqlite",
    ];
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let Some(name) = components.last() else {
        return true;
    };
    if components.iter().any(|part| DIRS.contains(&part.as_str())) {
        return true;
    }
    let in_gray_state = components
        .iter()
        .any(|part| part == "gray-telegram" || part == ".gray");
    if in_gray_state && !components.iter().any(|part| part == "work") {
        return true;
    }
    if FILES.contains(&name.as_str()) || name.starts_with(".env") {
        return true;
    }
    name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".kdbx")
}

fn infer_media_type(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "mov" | "m4v" => "video/mp4",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "json" => "application/json",
        "txt" | "md" | "log" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Read deliverable files into uploads with their names and media types.
pub fn load(paths: &[PathBuf]) -> Vec<Upload> {
    let mut uploads = Vec::new();
    for path in paths {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.is_empty() || bytes.len() as u64 > MAX_UPLOAD_BYTES {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        uploads.push(Upload {
            media_type: infer_media_type(&name),
            name,
            bytes,
            path: path.clone(),
        });
    }
    uploads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_names_are_refused() {
        let p = Path::new("/home/x/.ssh/id_rsa");
        assert!(is_sensitive(p));
        assert!(is_sensitive(Path::new("/home/x/.env.local")));
        assert!(!is_sensitive(Path::new("/home/x/report.pdf")));
    }

    #[test]
    fn tidy_collapses_blanks() {
        assert_eq!(tidy("a\n\n\n\nb\n"), "a\n\nb");
    }
}
