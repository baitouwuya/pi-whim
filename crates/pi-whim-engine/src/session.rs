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

pub(crate) fn pi_agent_directory() -> Result<PathBuf, String> {
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

/// Inverse of [`prompt_with_attachment_paths`], for transcripts read back from
/// Pi: a prompt's attachment paths travel as its trailing lines, so reloading
/// a session rebuilds the chips from them. A trailing line only converts when
/// it names an absolute path that still exists — anything else is prose the
/// author meant to send, and a deleted file's path is better shown than
/// dropped.
pub fn split_attachment_paths(text: &str) -> (String, Vec<Attachment>) {
    let generated_root = pi_whim_persistence::AttachmentStore::default_root();
    let mut end = text.len();
    let mut attachments = Vec::new();
    loop {
        let head = text[..end].trim_end();
        if head.is_empty() {
            break;
        }
        let line = head.rsplit('\n').next().unwrap_or(head);
        let candidate = Path::new(line);
        if !candidate.is_absolute() {
            break;
        }
        let generated = candidate.starts_with(&generated_root);
        let Ok(attachment) = attachment_from_path(candidate, generated) else {
            break;
        };
        attachments.push(attachment);
        end = head.len() - line.len();
    }
    attachments.reverse();
    (text[..end].trim_end().to_owned(), attachments)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_paths_come_off_as_attachments_in_order() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("one.png");
        let second = temporary.path().join("two.txt");
        fs::write(&first, "a").unwrap();
        fs::write(&second, "b").unwrap();
        let wire = prompt_with_attachment_paths(
            "look at these",
            &[
                attachment_from_path(&first, false).unwrap(),
                attachment_from_path(&second, false).unwrap(),
            ],
        );

        let (text, attachments) = split_attachment_paths(&wire);

        assert_eq!(text, "look at these");
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].name, "one.png");
        assert_eq!(attachments[1].name, "two.txt");
        assert!(!attachments[0].generated_by_app);
    }

    #[test]
    fn a_prompt_of_only_paths_leaves_no_text() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("only.png");
        fs::write(&file, "a").unwrap();
        let wire = prompt_with_attachment_paths("", &[attachment_from_path(&file, false).unwrap()]);

        let (text, attachments) = split_attachment_paths(&wire);

        assert!(text.is_empty());
        assert_eq!(attachments.len(), 1);
    }

    #[test]
    fn prose_relative_paths_and_missing_files_stay_text() {
        // Nothing here is an existing absolute path, so nothing converts.
        let prose = "see /definitely/not/here.png\nand relative/path.rs";
        let (text, attachments) = split_attachment_paths(prose);
        assert_eq!(text, prose);
        assert!(attachments.is_empty());

        // A path that no longer exists stays visible rather than vanishing.
        let missing = "question\n/definitely/not/here.png";
        let (text, attachments) = split_attachment_paths(missing);
        assert_eq!(text, missing);
        assert!(attachments.is_empty());
    }
}
