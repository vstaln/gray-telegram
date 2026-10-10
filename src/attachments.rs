//! Inbound Telegram files. A photo/document arrives as a `file_id`; the bot
//! asks `getFile` for the temporary path, downloads it, saves the file under
//! workdir and names it in the prompt — gray reads it with its own tools
//! (`cat <image>` is its vision path, cat for text), so no decoding happens
//! here. Cap, don't stream: a 20MB+ attachment is a deny (the public Bot
//! API will not serve it anyway), not a memory spike.

use std::path::{Path, PathBuf};

use crate::transport::{Rest, MAX_DOWNLOAD_BYTES};

pub async fn save(
    rest: &Rest,
    file_id: &str,
    filename: &str,
    msg_id: &str,
    workdir: &Path,
    max_bytes: u64,
) -> Option<PathBuf> {
    let max = max_bytes.min(MAX_DOWNLOAD_BYTES);
    let (file_path, size) = rest.get_file(file_id).await.ok()?;
    if size > max {
        return None;
    }
    let safe: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let safe = if safe.is_empty() { "file".to_string() } else { safe };
    let dir = workdir.join("attachments");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{msg_id}-{safe}"));
    let bytes = rest.download(&file_path, max).await.ok()?;
    std::fs::write(&path, &bytes).ok()?;
    Some(path)
}

/// How the prompt names what the user attached. One line per file, path
/// first — that is what gray's tools consume.
pub fn prompt_lines(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("[attached file: {}]", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}
