//! Opt-in capability sharing.
//! Session/work state stays separate; this is not a sandbox.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Context cap: 128 KiB total across all shared context files.
const CONTEXT_CAP: usize = 128 * 1024;

/// Require an absolute path that exists (else the exact Python message).
pub fn absolute(v: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(v);
    if !path.is_absolute() || !path.exists() {
        return Err("Shared capability paths must be absolute and exist".to_string());
    }
    Ok(path)
}

/// Single-quote shell escaping (shlex.join parity for one argv array).
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Link whitelisted skills, inline shared context, and write sidecar
/// launchers for shared plugins. Returns the `work/gray.yml` profile lines.
pub fn prepare(config: &Value, home: &Path) -> Result<String, String> {
    let skills = home.join("skills");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&skills)
        .map_err(|_| "cannot create skills directory".to_string())?;
    let _ = std::fs::set_permissions(&skills, std::fs::Permissions::from_mode(0o700));

    // Desired links keyed by hex sha256 of the path string (Python parity).
    let mut desired: std::collections::BTreeMap<String, PathBuf> = Default::default();
    if let Some(list) = config.get("shared_skills").and_then(Value::as_array) {
        for v in list {
            let text = v.as_str().unwrap_or("");
            let path = absolute(text)?;
            if !path.join("SKILL.md").is_file() {
                return Err("Shared skill must be a directory containing SKILL.md".to_string());
            }
            let mut h = Sha256::new();
            h.update(path.to_string_lossy().as_bytes());
            desired.insert(hex::encode(h.finalize()), path);
        }
    }
    // Python: `mkdir(exist_ok=True)` leaves stale links to removed skills in
    // place until the second lookup pass; prune symlinks outside the set.
    for entry in std::fs::read_dir(&skills).map_err(|_| "cannot list skills".to_string())? {
        let entry = entry.map_err(|_| "cannot list skills".to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_symlink() && !desired.contains_key(&name) {
            std::fs::remove_file(entry.path()).map_err(|_| "cannot prune skill".to_string())?;
        }
    }
    for (name, path) in &desired {
        let link = skills.join(name);
        if link.is_symlink() {
            continue;
        }
        if link.exists() {
            return Err("Shared skill path collision; refusing to overwrite".to_string());
        }
        std::os::unix::fs::symlink(path, &link).map_err(|_| "cannot link skill".to_string())?;
    }

    // Context: capped total, UTF-8 required, AGENTS.md + marker file.
    let mut context: Vec<String> = Vec::new();
    let mut total = 0usize;
    if let Some(list) = config.get("shared_context").and_then(Value::as_array) {
        for v in list {
            let text = v.as_str().unwrap_or("");
            let path = absolute(text)?;
            let raw =
                std::fs::read(&path).map_err(|_| format!("cannot read {}", path.display()))?;
            total += raw.len().min(CONTEXT_CAP + 1);
            if total > CONTEXT_CAP {
                return Err("Shared context exceeds 128 KiB; select smaller files".to_string());
            }
            let text =
                String::from_utf8(raw).map_err(|_| format!("cannot read {}", path.display()))?;
            context.push(text);
        }
    }
    let marker = home.join("shared-context.json");
    if !context.is_empty() {
        let agents = "You are gray. Follow the user request. Never disclose secrets.\n\n"
            .to_string()
            + &context.join("\n\n");
        std::fs::write(home.join("AGENTS.md"), agents)
            .map_err(|_| "cannot write shared context".to_string())?;
        std::fs::write(&marker, "{}\n").map_err(|_| "cannot write shared context".to_string())?;
    } else if marker.exists() {
        let _ = std::fs::remove_file(home.join("AGENTS.md"));
        std::fs::remove_file(&marker).map_err(|_| "cannot clear shared context".to_string())?;
    }

    // Shared plugins: validated argv arrays → 0700 launchers → profile lines.
    let mut profile = String::new();
    if let Some(list) = config.get("shared_plugins").and_then(Value::as_array) {
        for (index, argv) in list.iter().enumerate() {
            let args: Vec<String> = match argv.as_array() {
                Some(a) if !a.is_empty() => {
                    let mut out = Vec::with_capacity(a.len());
                    for item in a {
                        match item.as_str() {
                            Some(s) if !s.contains('\0') => out.push(s.to_string()),
                            _ => {
                                return Err(
                                    "Shared plugins require nonempty argv arrays".to_string()
                                );
                            }
                        }
                    }
                    out
                }
                _ => return Err("Shared plugins require nonempty argv arrays".to_string()),
            };
            if args[0].is_empty() {
                return Err("Shared plugin executable is empty".to_string());
            }
            let launcher = home.join(format!("shared-plugin-{index}"));
            let script = format!("#!/bin/sh\nexec {}\n", shell_join(&args));
            std::fs::write(&launcher, script)
                .map_err(|_| "cannot write shared plugin".to_string())?;
            std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "cannot protect shared plugin".to_string())?;
            profile += &format!(
                "  - sidecar: {}\n",
                serde_json::to_string(&launcher.to_string_lossy()).unwrap_or_default()
            );
        }
    }
    Ok(profile)
}

/// Lowercase hex of raw bytes (sha256 digests for link names).
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}
