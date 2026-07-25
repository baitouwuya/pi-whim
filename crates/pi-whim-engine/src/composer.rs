//! The prompt being drafted, and its attachments.
//!
//! This was three fields on `AppState` — `composer`, `composer_attachments`,
//! and `search` — reached through five reducer actions. None of it is domain
//! state: nothing outside the view ever read it, and a draft that has not been
//! submitted has no bearing on the session. Keeping it in the reducer meant
//! every view mutation went through a dispatch, and the egui view ended up
//! writing `state.composer` directly in twenty places anyway.
//!
//! It lives here rather than in a view crate so both presentation layers share
//! one implementation, including the attachment de-duplication that has a test.

use pi_whim_core::Attachment;

/// A prompt draft: its text, its attachments, and the model-picker filter.
#[derive(Clone, Debug, Default)]
pub struct Composer {
    text: String,
    attachments: Vec<Attachment>,
    /// Filter text for the model picker. Transient, and cleared when the picker
    /// closes.
    search: String,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Mutable access for a text input to bind against directly.
    pub fn text_mut(&mut self) -> &mut String {
        &mut self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    pub fn clear_text(&mut self) {
        self.text.clear();
    }

    /// Whether there is anything worth submitting.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.attachments.is_empty()
    }

    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    /// Add `attachment` unless its path is already attached.
    ///
    /// The same file arriving twice — dropped and then pasted, say — should not
    /// be sent twice.
    pub fn add_attachment(&mut self, attachment: Attachment) {
        if !self
            .attachments
            .iter()
            .any(|existing| existing.path == attachment.path)
        {
            self.attachments.push(attachment);
        }
    }

    pub fn remove_attachment(&mut self, path: &str) {
        self.attachments
            .retain(|attachment| attachment.path != path);
    }

    pub fn clear_attachments(&mut self) {
        self.attachments.clear();
    }

    /// Take the draft for submission, leaving it empty.
    pub fn take(&mut self) -> (String, Vec<Attachment>) {
        (
            std::mem::take(&mut self.text),
            std::mem::take(&mut self.attachments),
        )
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    pub fn search_mut(&mut self) -> &mut String {
        &mut self.search
    }

    pub fn set_search(&mut self, search: impl Into<String>) {
        self.search = search.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::AttachmentKind;

    fn attachment(path: &str) -> Attachment {
        Attachment {
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.into(),
            kind: AttachmentKind::File,
            generated_by_app: false,
        }
    }

    #[test]
    fn attachments_are_deduplicated_by_path() {
        // The same file can arrive twice — dropped, then pasted.
        let mut composer = Composer::new();
        composer.add_attachment(attachment("/tmp/example.txt"));
        composer.add_attachment(attachment("/tmp/example.txt"));

        assert_eq!(composer.attachments().len(), 1);
    }

    #[test]
    fn attachments_are_removable_by_path() {
        let mut composer = Composer::new();
        composer.add_attachment(attachment("/tmp/example.txt"));
        composer.add_attachment(attachment("/tmp/other.txt"));

        composer.remove_attachment("/tmp/example.txt");

        assert_eq!(composer.attachments().len(), 1);
        assert_eq!(composer.attachments()[0].path, "/tmp/other.txt");
    }

    #[test]
    fn a_draft_with_only_whitespace_is_empty() {
        let mut composer = Composer::new();
        assert!(composer.is_empty());

        composer.set_text("   \n\t ");
        assert!(composer.is_empty());

        composer.set_text("hello");
        assert!(!composer.is_empty());
    }

    #[test]
    fn attachments_alone_are_worth_submitting() {
        let mut composer = Composer::new();
        composer.add_attachment(attachment("/tmp/image.png"));

        assert!(!composer.is_empty());
    }

    #[test]
    fn taking_the_draft_leaves_it_empty() {
        let mut composer = Composer::new();
        composer.set_text("hello");
        composer.add_attachment(attachment("/tmp/example.txt"));

        let (text, attachments) = composer.take();

        assert_eq!(text, "hello");
        assert_eq!(attachments.len(), 1);
        assert!(composer.is_empty());
        assert!(composer.attachments().is_empty());
    }

    #[test]
    fn search_is_independent_of_the_draft() {
        // Clearing a submitted prompt should not disturb the model filter.
        let mut composer = Composer::new();
        composer.set_search("opus");
        composer.set_text("hello");

        composer.clear_text();

        assert_eq!(composer.search(), "opus");
    }
}
