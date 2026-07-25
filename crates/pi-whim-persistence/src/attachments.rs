use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use image::ImageEncoder;
use pi_whim_core::{Attachment, AttachmentKind};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PASTED_TEXT_NAME: &str = "pasted-text.txt";
const PASTED_IMAGE_NAME: &str = "pasted-image.png";
const MANIFEST_NAME: &str = "pasted-text-attachments.json";

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentManifest {
    #[serde(default)]
    attachment_paths: BTreeSet<String>,
    #[serde(default)]
    pending_removal_paths: BTreeSet<String>,
    #[serde(default)]
    text_excerpts_by_path: BTreeMap<String, String>,
}

/// Tracks only clipboard files created by Pi-Whim. External attachments never
/// enter this manifest and are consequently never candidates for deletion.
pub struct AttachmentStore {
    root: PathBuf,
    manifest_path: PathBuf,
    manifest: AttachmentManifest,
    startup_error: Option<String>,
}

impl AttachmentStore {
    pub fn open_default() -> Self {
        let root = dirs::data_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("pi-whim")
            .join("attachments");
        Self::open(root.clone()).unwrap_or_else(|error| Self {
            manifest_path: root.join(MANIFEST_NAME),
            root,
            manifest: AttachmentManifest::default(),
            startup_error: Some(error),
        })
    }

    pub fn open(root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let manifest_path = root.join(MANIFEST_NAME);
        let manifest = if manifest_path.exists() {
            let contents = fs::read_to_string(&manifest_path).map_err(|error| error.to_string())?;
            serde_json::from_str(&contents).map_err(|error| error.to_string())?
        } else {
            AttachmentManifest::default()
        };
        let mut store = Self {
            root,
            manifest_path,
            manifest,
            startup_error: None,
        };
        store.retry_pending_removals()?;
        Ok(store)
    }

    pub fn create_pasted_text(&mut self, text: &str) -> Result<Attachment, String> {
        self.check_ready()?;
        self.create_generated_file(PASTED_TEXT_NAME, text.as_bytes(), Some(text))
    }

    pub fn create_pasted_image(
        &mut self,
        width: usize,
        height: usize,
        rgba: &[u8],
    ) -> Result<Attachment, String> {
        self.check_ready()?;
        let width = u32::try_from(width).map_err(|_| "Pasted image is too wide.".to_owned())?;
        let height = u32::try_from(height).map_err(|_| "Pasted image is too tall.".to_owned())?;
        let expected_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "Pasted image is too large.".to_owned())?;
        if rgba.len() != expected_len {
            return Err("Clipboard image pixels are invalid.".into());
        }
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
            .map_err(|error| error.to_string())?;
        self.create_generated_file(PASTED_IMAGE_NAME, &png, None)
    }

    fn create_generated_file(
        &mut self,
        name: &str,
        bytes: &[u8],
        text_excerpt: Option<&str>,
    ) -> Result<Attachment, String> {
        let directory = self.root.join(Uuid::new_v4().to_string());
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let path = directory.join(name);
        atomic_write(&path, bytes)?;
        let path = canonical_attachment_path(&path)?;
        let path_string = path.to_string_lossy().into_owned();
        self.manifest.attachment_paths.insert(path_string.clone());
        if let Some(text) = text_excerpt {
            self.manifest
                .text_excerpts_by_path
                .insert(path_string.clone(), text.chars().take(240).collect());
        }
        if let Err(error) = self.write_manifest() {
            let _ = delete_owned_path(&self.root, &path);
            self.manifest.attachment_paths.remove(&path_string);
            self.manifest.text_excerpts_by_path.remove(&path_string);
            return Err(error);
        }
        Ok(Attachment {
            name: name.into(),
            path: path_string,
            kind: AttachmentKind::File,
            generated_by_app: true,
        })
    }

    pub fn remove_generated(&mut self, path: &str) -> Result<(), String> {
        self.check_ready()?;
        let path = PathBuf::from(path);
        let path_string = path.to_string_lossy().into_owned();
        if !self.manifest.attachment_paths.remove(&path_string) {
            return Ok(());
        }
        self.manifest.text_excerpts_by_path.remove(&path_string);
        match delete_owned_path(&self.root, &path) {
            Ok(()) => {
                self.manifest.pending_removal_paths.remove(&path_string);
            }
            Err(_) => {
                self.manifest.pending_removal_paths.insert(path_string);
            }
        }
        self.write_manifest()
    }

    fn retry_pending_removals(&mut self) -> Result<(), String> {
        let pending = std::mem::take(&mut self.manifest.pending_removal_paths);
        for path in pending {
            match delete_owned_path(&self.root, Path::new(&path)) {
                Ok(()) => {
                    self.manifest.text_excerpts_by_path.remove(&path);
                }
                Err(_) => {
                    self.manifest.pending_removal_paths.insert(path);
                }
            }
        }
        self.write_manifest()
    }

    fn check_ready(&self) -> Result<(), String> {
        self.startup_error.clone().map_or(Ok(()), Err)
    }

    fn write_manifest(&self) -> Result<(), String> {
        let contents =
            serde_json::to_vec_pretty(&self.manifest).map_err(|error| error.to_string())?;
        atomic_write(&self.manifest_path, &contents)
    }
}

fn canonical_attachment_path(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize().map_err(|error| error.to_string())
}

fn delete_owned_path(root: &Path, path: &Path) -> Result<(), String> {
    let root = canonical_attachment_path(root)?;
    let path = if path.exists() {
        canonical_attachment_path(path)?
    } else {
        path.to_path_buf()
    };
    if !path.starts_with(&root)
        || !matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(PASTED_TEXT_NAME | PASTED_IMAGE_NAME)
        )
    {
        return Err("refusing to delete a non-generated attachment".into());
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    if let Some(parent) = path.parent()
        && let Err(error) = fs::remove_dir(parent)
        && error.kind() != std::io::ErrorKind::NotFound
        && error.kind() != std::io::ErrorKind::DirectoryNotEmpty
    {
        return Err(error.to_string());
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, contents).map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_text_is_owned_and_removed_without_touching_external_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("attachments");
        let external = temporary.path().join("external.txt");
        fs::write(&external, "external").unwrap();
        let mut store = AttachmentStore::open(root).unwrap();
        let attachment = store.create_pasted_text("large paste").unwrap();

        store.remove_generated(&attachment.path).unwrap();
        assert!(!Path::new(&attachment.path).exists());
        assert!(external.exists());
    }

    #[test]
    fn manifest_survives_reopen() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("attachments");
        let path = {
            let mut store = AttachmentStore::open(root.clone()).unwrap();
            store.create_pasted_text("retained").unwrap().path
        };
        let store = AttachmentStore::open(root).unwrap();
        assert!(store.manifest.attachment_paths.contains(&path));
    }

    #[test]
    fn generated_clipboard_image_is_a_removable_png_attachment() {
        let temporary = tempfile::tempdir().unwrap();
        let mut store = AttachmentStore::open(temporary.path().join("attachments")).unwrap();
        let attachment = store
            .create_pasted_image(1, 1, &[0, 128, 255, 255])
            .unwrap();

        assert_eq!(attachment.name, PASTED_IMAGE_NAME);
        assert!(Path::new(&attachment.path).starts_with(temporary.path().canonicalize().unwrap()));
        assert!(
            fs::read(&attachment.path)
                .unwrap()
                .starts_with(b"\x89PNG\r\n\x1a\n")
        );
        store.remove_generated(&attachment.path).unwrap();
        assert!(!Path::new(&attachment.path).exists());
    }
}
