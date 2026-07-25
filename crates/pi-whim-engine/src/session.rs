//! Session launch prerequisites and prompt assembly.
//!
//! Everything a Pi process needs staged before it starts — its agent
//! directory, the bundled agent-team extension, the bash policy name it reads
//! from the environment — plus the attachment handling that turns dropped or
//! pasted files into prompt text.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use pi_whim_core::{Attachment, AttachmentKind, BashPolicy};

pub fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn pi_agent_directory() -> Result<PathBuf, String> {
    let root = dirs::data_dir()
        .ok_or_else(|| "Application Support directory is unavailable.".to_owned())?
        .join("pi-whim")
        .join("agent");
    Ok(root)
}

pub fn bash_policy_name(policy: &BashPolicy) -> &'static str {
    match policy {
        BashPolicy::Allow => "allow",
        BashPolicy::Ask => "ask",
        BashPolicy::Deny => "deny",
    }
}

pub fn attachment_from_path(path: &Path, generated_by_app: bool) -> Result<Attachment, String> {
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    let metadata = path.metadata().map_err(|error| error.to_string())?;
    let kind = if metadata.is_dir() {
        AttachmentKind::Directory
    } else {
        AttachmentKind::File
    };
    Ok(Attachment {
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment")
            .into(),
        path: path.to_string_lossy().into_owned(),
        kind,
        generated_by_app,
    })
}

pub fn is_large_paste(text: &str) -> bool {
    text.chars().count() > 1_000 || text.lines().count() > 10
}

pub fn prompt_with_attachment_paths(content: &str, attachments: &[Attachment]) -> String {
    let paths = attachments
        .iter()
        .map(|attachment| attachment.path.as_str())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return content.to_owned();
    }
    if content.is_empty() {
        paths.join("\n")
    } else {
        format!("{content}\n{}", paths.join("\n"))
    }
}

pub fn ensure_agent_team_extension(sessions_path: &Path) -> std::io::Result<PathBuf> {
    let directory = sessions_path.join(".pi-whim-agent-team-extension");
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("client.ts"),
        include_str!("../../../extensions/agent-team/client.ts"),
    )?;
    let entrypoint = directory.join("index.ts");
    fs::write(
        &entrypoint,
        include_str!("../../../extensions/agent-team/index.ts"),
    )?;
    Ok(entrypoint)
}
