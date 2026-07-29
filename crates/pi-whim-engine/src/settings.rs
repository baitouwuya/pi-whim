//! Form state for the settings page.
//!
//! A provider or a search engine is edited before it is saved, so there is a
//! draft that is neither the stored profile nor a domain action. That draft also
//! decides what the page can do: whether Save is reachable, whether the name
//! collides with an existing one, what a preset fills in.
//!
//! None of that is presentation, and none of it belongs to one UI framework —
//! these types lived in the egui crate, which meant the gpui page would have had
//! to reimplement the same validation and get it subtly different.

use pi_whim_core::{
    ProviderId, ProviderModel, ProviderProfile, ProviderProtocol, SearchEngineId, SearchEngineKind,
    SearchEngineProfile, provider_name_key,
};

/// Which page of settings is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Section {
    #[default]
    General,
    Execution,
    Providers,
    WebSearch,
}

impl Section {
    /// Every section, in the order they are listed.
    ///
    /// General first because it is what most readers came for; execution follows
    /// because it controls local behavior; providers precede optional web search.
    pub const ALL: [Self; 4] = [
        Self::General,
        Self::Execution,
        Self::Providers,
        Self::WebSearch,
    ];

    /// The translation key for this section's name.
    pub fn key(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Execution => "execution",
            Self::Providers => "providers",
            Self::WebSearch => "web-search",
        }
    }
}

/// A known provider, which fills in the connection fields.
///
/// Base URLs and protocols are the two things readers get wrong, and neither is
/// guessable. Custom exists so a self-hosted endpoint is still reachable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preset {
    #[default]
    Custom,
    OpenAi,
    Anthropic,
    Google,
    OpenRouter,
}

impl Preset {
    pub const ALL: [Self; 5] = [
        Self::Custom,
        Self::OpenAi,
        Self::Anthropic,
        Self::Google,
        Self::OpenRouter,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Custom => "Custom",
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Google => "Google / Gemini",
            Self::OpenRouter => "OpenRouter",
        }
    }

    /// What this preset fills in, or `None` for Custom.
    pub fn connection(self) -> Option<(&'static str, &'static str, ProviderProtocol)> {
        match self {
            // Custom leaves whatever was typed alone: picking it should not wipe
            // a URL the reader entered by hand.
            Self::Custom => None,
            Self::OpenAi => Some((
                "OpenAI",
                "https://api.openai.com/v1",
                ProviderProtocol::OpenAiResponses,
            )),
            Self::Anthropic => Some((
                "Anthropic",
                "https://api.anthropic.com",
                ProviderProtocol::AnthropicMessages,
            )),
            Self::Google => Some((
                "Google / Gemini",
                "https://generativelanguage.googleapis.com/v1beta",
                ProviderProtocol::GoogleGenerativeAi,
            )),
            Self::OpenRouter => Some((
                "OpenRouter",
                "https://openrouter.ai/api/v1",
                ProviderProtocol::OpenAiCompletions,
            )),
        }
    }

    /// Fill `draft` in from this preset.
    pub fn apply(self, draft: &mut ProviderDraft) {
        let Some((name, base_url, protocol)) = self.connection() else {
            return;
        };
        draft.name = name.into();
        draft.base_url = base_url.into();
        draft.protocol = protocol;
    }
}

/// A provider being edited.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderDraft {
    /// `None` for one that has not been saved yet.
    pub id: Option<ProviderId>,
    pub name: String,
    pub base_url: String,
    pub protocol: ProviderProtocol,
    pub preset: Preset,
    /// What was typed into the key field. Empty means "leave the stored one".
    pub api_key: String,
    /// Whether a key is already in the keychain for this provider.
    pub has_api_key: bool,
    pub models: Vec<ProviderModel>,
    /// The model id being added by hand.
    pub manual_model_id: String,
}

impl Default for ProviderDraft {
    fn default() -> Self {
        Self {
            id: None,
            name: "OpenAI-compatible".into(),
            base_url: ProviderProtocol::OpenAiCompletions
                .default_base_url()
                .into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            preset: Preset::Custom,
            api_key: String::new(),
            has_api_key: false,
            models: Vec::new(),
            manual_model_id: String::new(),
        }
    }
}

impl ProviderDraft {
    pub fn from_profile(profile: &ProviderProfile) -> Self {
        Self {
            id: Some(profile.id),
            name: profile.name.clone(),
            base_url: profile.base_url.clone(),
            protocol: profile.protocol,
            // Not inferred back from the URL: the reader chose these values, and
            // showing a preset they did not pick would misreport where they came
            // from.
            preset: Preset::Custom,
            // Never read back out of the keychain to fill a field. The stored key
            // stays stored; an empty field means "unchanged".
            api_key: String::new(),
            has_api_key: profile.has_api_key,
            models: profile.models.clone(),
            manual_model_id: String::new(),
        }
    }

    /// The profile this draft would store.
    ///
    /// `now_ms` is passed in rather than read: the engine has no clock of its own
    /// and this keeps the conversion testable.
    pub fn to_profile(&self, now_ms: i64) -> ProviderProfile {
        ProviderProfile {
            id: self.id.unwrap_or_else(uuid::Uuid::new_v4),
            name: self.name.trim().to_owned(),
            base_url: self.base_url.trim().trim_end_matches('/').to_owned(),
            protocol: self.protocol,
            models: self.models.clone(),
            updated_at_ms: now_ms,
            has_api_key: self.has_api_key || !self.api_key.trim().is_empty(),
        }
    }

    /// The key to store, if one was typed.
    pub fn typed_api_key(&self) -> Option<String> {
        let trimmed = self.api_key.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Whether another stored provider already uses this name.
    ///
    /// Names are how a model is attributed in the conversation, so two providers
    /// sharing one would make the attribution ambiguous. Compared by
    /// `provider_name_key`, which is what the store uses.
    pub fn name_collides(&self, existing: &[ProviderProfile]) -> bool {
        let key = provider_name_key(&self.name);
        if key.is_empty() {
            return false;
        }
        existing
            .iter()
            .any(|profile| Some(profile.id) != self.id && provider_name_key(&profile.name) == key)
    }

    /// Whether this draft is worth saving.
    ///
    /// A provider with no models cannot answer anything, and one with no URL
    /// cannot be reached, so both are refused before the store is touched.
    pub fn can_save(&self, existing: &[ProviderProfile]) -> bool {
        !self.name.trim().is_empty()
            && !self.base_url.trim().is_empty()
            && !self.models.is_empty()
            && !self.name_collides(existing)
    }

    /// Whether asking the provider for its model list would work.
    pub fn can_discover(&self) -> bool {
        !self.base_url.trim().is_empty()
    }

    /// Add the model id typed by hand, and clear the field.
    ///
    /// Returns whether anything was added. Duplicates are dropped rather than
    /// appended: the same id twice would show as two identical rows.
    pub fn add_manual_model(&mut self) -> bool {
        let id = self.manual_model_id.trim().to_owned();
        self.manual_model_id.clear();
        if id.is_empty() || self.models.iter().any(|model| model.id == id) {
            return false;
        }
        self.models.push(ProviderModel::new(id));
        true
    }

    /// Change the protocol, moving the base URL with it when it was the default.
    ///
    /// A URL the reader typed is left alone; one that was only there because of
    /// the previous protocol follows the new one, since it would otherwise be
    /// silently wrong.
    pub fn set_protocol(&mut self, protocol: ProviderProtocol) {
        let previous = self.protocol;
        self.protocol = protocol;
        if self.base_url.trim() == previous.default_base_url() {
            self.base_url = protocol.default_base_url().into();
        }
    }
}

/// A search engine being edited.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchEngineDraft {
    pub id: Option<SearchEngineId>,
    pub name: String,
    pub kind: SearchEngineKind,
    pub base_url: String,
    pub enabled: bool,
    /// What was typed into the key field. Empty means "leave the stored one".
    pub api_key: String,
    /// Whether a key is already in Keychain for this engine.
    pub has_api_key: bool,
}

impl Default for SearchEngineDraft {
    fn default() -> Self {
        Self {
            id: None,
            name: SearchEngineKind::Searxng.default_name().into(),
            kind: SearchEngineKind::Searxng,
            base_url: String::new(),
            enabled: true,
            api_key: String::new(),
            has_api_key: false,
        }
    }
}

impl SearchEngineDraft {
    pub fn from_profile(profile: &SearchEngineProfile) -> Self {
        Self {
            id: Some(profile.id),
            name: profile.name.clone(),
            kind: profile.kind,
            base_url: profile.base_url.clone(),
            enabled: profile.enabled,
            // Never read the secret back into an editable field.
            api_key: String::new(),
            has_api_key: profile.has_api_key,
        }
    }

    pub fn to_profile(&self, position: u32) -> SearchEngineProfile {
        SearchEngineProfile {
            id: self.id.unwrap_or_else(uuid::Uuid::new_v4),
            name: self.name.trim().to_owned(),
            kind: self.kind,
            base_url: self.base_url.trim().trim_end_matches('/').to_owned(),
            enabled: self.enabled,
            position,
            has_api_key: self.has_api_key || !self.api_key.trim().is_empty(),
        }
    }

    pub fn typed_api_key(&self) -> Option<String> {
        let trimmed = self.api_key.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Change adapter, moving untouched defaults with it.
    pub fn set_kind(&mut self, kind: SearchEngineKind) {
        let previous = self.kind;
        self.kind = kind;
        if self.name.trim() == previous.default_name() {
            self.name = kind.default_name().into();
        }
        if self.base_url.trim().trim_end_matches('/') == previous.default_base_url() {
            self.base_url = kind.default_base_url().into();
        }
        if !kind.requires_api_key() {
            self.api_key.clear();
            self.has_api_key = false;
        }
    }

    /// Whether this draft is worth saving.
    pub fn can_save(&self) -> bool {
        !self.name.trim().is_empty()
            && !self.base_url.trim().is_empty()
            && (!self.kind.requires_api_key()
                || self.has_api_key
                || !self.api_key.trim().is_empty())
    }
}

/// Store `draft` into `profiles`, replacing the one it edits or appending.
///
/// Returns the list to save. Positions are renumbered from the order in the list,
/// which is what the reordering buttons manipulate.
pub fn upsert_search_engine(
    profiles: &[SearchEngineProfile],
    draft: &SearchEngineDraft,
) -> Vec<SearchEngineProfile> {
    let mut profiles = profiles.to_vec();
    let profile = draft.to_profile(profiles.len() as u32);
    match profiles
        .iter()
        .position(|existing| existing.id == profile.id)
    {
        Some(index) => profiles[index] = profile,
        None => profiles.push(profile),
    }
    renumber(profiles)
}

/// Drop the engine at `index`.
pub fn remove_search_engine(
    profiles: &[SearchEngineProfile],
    index: usize,
) -> Vec<SearchEngineProfile> {
    let mut profiles = profiles.to_vec();
    if index < profiles.len() {
        profiles.remove(index);
    }
    renumber(profiles)
}

/// Move the engine at `index` one place towards `delta`.
///
/// Order is what decides which engine answers a search first, so it is worth
/// editing. Moving past either end does nothing rather than wrapping: a list
/// that jumps from top to bottom under one click is hard to aim.
pub fn move_search_engine(
    profiles: &[SearchEngineProfile],
    index: usize,
    delta: isize,
) -> Vec<SearchEngineProfile> {
    let mut profiles = profiles.to_vec();
    let target = index as isize + delta;
    if index >= profiles.len() || target < 0 || target as usize >= profiles.len() {
        return profiles;
    }
    profiles.swap(index, target as usize);
    renumber(profiles)
}

/// Renumber positions from list order.
fn renumber(mut profiles: Vec<SearchEngineProfile>) -> Vec<SearchEngineProfile> {
    for (position, profile) in profiles.iter_mut().enumerate() {
        profile.position = position as u32;
    }
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> ProviderProfile {
        ProviderProfile {
            id: uuid::Uuid::new_v4(),
            name: name.to_owned(),
            base_url: "https://example.com".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("m1")],
            updated_at_ms: 1,
            has_api_key: true,
        }
    }

    fn engine(name: &str, position: u32) -> SearchEngineProfile {
        SearchEngineProfile {
            id: uuid::Uuid::new_v4(),
            name: name.to_owned(),
            kind: SearchEngineKind::Searxng,
            base_url: format!("https://{name}.example"),
            enabled: true,
            position,
            has_api_key: false,
        }
    }

    /// A draft that would save.
    fn saveable() -> ProviderDraft {
        ProviderDraft {
            models: vec![ProviderModel::new("m1")],
            ..Default::default()
        }
    }

    #[test]
    fn a_preset_fills_in_the_two_fields_readers_get_wrong() {
        let mut draft = ProviderDraft::default();
        Preset::Anthropic.apply(&mut draft);

        assert_eq!(draft.name, "Anthropic");
        assert_eq!(draft.base_url, "https://api.anthropic.com");
        assert_eq!(draft.protocol, ProviderProtocol::AnthropicMessages);
    }

    #[test]
    fn custom_leaves_what_was_typed_alone() {
        // Picking Custom after typing a self-hosted URL must not wipe it.
        let mut draft = ProviderDraft {
            name: "My server".into(),
            base_url: "http://localhost:8080/v1".into(),
            ..Default::default()
        };
        let before = draft.clone();
        Preset::Custom.apply(&mut draft);

        assert_eq!(draft, before);
    }

    #[test]
    fn every_preset_but_custom_names_a_connection() {
        for preset in Preset::ALL {
            assert_eq!(preset.connection().is_none(), preset == Preset::Custom);
        }
    }

    #[test]
    fn a_stored_key_is_never_read_back_into_the_field() {
        // Filling the field from the keychain would put the secret on screen and
        // risk writing it back mangled.
        let draft = ProviderDraft::from_profile(&profile("OpenAI"));

        assert!(draft.api_key.is_empty());
        assert!(draft.has_api_key);
    }

    #[test]
    fn an_untouched_key_field_keeps_the_stored_key() {
        let draft = ProviderDraft::from_profile(&profile("OpenAI"));

        assert_eq!(draft.typed_api_key(), None);
        assert!(draft.to_profile(1).has_api_key);
    }

    #[test]
    fn a_typed_key_is_trimmed() {
        // Pasted keys arrive with whitespace, and a key with a trailing newline
        // fails to authenticate in a way that is hard to see.
        let draft = ProviderDraft {
            api_key: "  sk-123\n".into(),
            ..Default::default()
        };

        assert_eq!(draft.typed_api_key().as_deref(), Some("sk-123"));
    }

    #[test]
    fn saving_trims_the_name_and_drops_a_trailing_slash() {
        // "https://x/" and "https://x" are the same endpoint; storing both would
        // make the same provider look like two.
        let draft = ProviderDraft {
            name: "  OpenAI  ".into(),
            base_url: "https://api.openai.com/v1/".into(),
            ..saveable()
        };
        let stored = draft.to_profile(7);

        assert_eq!(stored.name, "OpenAI");
        assert_eq!(stored.base_url, "https://api.openai.com/v1");
        assert_eq!(stored.updated_at_ms, 7);
    }

    #[test]
    fn a_new_provider_gets_an_id_and_an_edited_one_keeps_its_own() {
        let existing = profile("OpenAI");
        let draft = ProviderDraft::from_profile(&existing);

        assert_eq!(draft.to_profile(1).id, existing.id);
        assert_ne!(ProviderDraft::default().to_profile(1).id, existing.id);
    }

    #[test]
    fn a_name_another_provider_already_uses_collides() {
        // Names attribute models in the conversation; two the same is ambiguous.
        let existing = vec![profile("OpenAI")];
        let draft = ProviderDraft {
            name: "OpenAI".into(),
            ..saveable()
        };

        assert!(draft.name_collides(&existing));
        assert!(!draft.can_save(&existing));
    }

    #[test]
    fn a_provider_does_not_collide_with_itself() {
        let existing = vec![profile("OpenAI")];
        let draft = ProviderDraft::from_profile(&existing[0]);

        assert!(!draft.name_collides(&existing));
    }

    #[test]
    fn collision_ignores_case_and_spacing() {
        // The store compares by `provider_name_key`, so the form has to agree or
        // a save would be rejected further down with no explanation.
        let existing = vec![profile("OpenAI")];
        let draft = ProviderDraft {
            name: "  openai ".into(),
            ..saveable()
        };

        assert!(draft.name_collides(&existing));
    }

    #[test]
    fn a_blank_name_is_not_a_collision_but_still_cannot_save() {
        let existing = vec![profile("OpenAI")];
        let draft = ProviderDraft {
            name: "   ".into(),
            ..saveable()
        };

        assert!(!draft.name_collides(&existing));
        assert!(!draft.can_save(&existing));
    }

    #[test]
    fn a_provider_with_no_models_cannot_save() {
        // It could not answer anything, so the store is not touched.
        let draft = ProviderDraft::default();

        assert!(draft.models.is_empty());
        assert!(!draft.can_save(&[]));
    }

    #[test]
    fn a_provider_with_no_url_cannot_save_or_be_discovered() {
        let draft = ProviderDraft {
            base_url: "   ".into(),
            ..saveable()
        };

        assert!(!draft.can_save(&[]));
        assert!(!draft.can_discover());
    }

    #[test]
    fn a_complete_draft_saves() {
        assert!(saveable().can_save(&[]));
    }

    #[test]
    fn a_model_added_by_hand_clears_the_field() {
        let mut draft = ProviderDraft {
            manual_model_id: "  gpt-5 ".into(),
            ..Default::default()
        };

        assert!(draft.add_manual_model());
        assert_eq!(draft.models.len(), 1);
        assert_eq!(draft.models[0].id, "gpt-5");
        assert!(draft.manual_model_id.is_empty());
    }

    #[test]
    fn the_same_model_twice_is_added_once() {
        // Two identical rows read as a bug in the list.
        let mut draft = ProviderDraft {
            manual_model_id: "gpt-5".into(),
            ..Default::default()
        };
        draft.add_manual_model();
        draft.manual_model_id = "gpt-5".into();

        assert!(!draft.add_manual_model());
        assert_eq!(draft.models.len(), 1);
    }

    #[test]
    fn an_empty_model_field_adds_nothing() {
        let mut draft = ProviderDraft {
            manual_model_id: "   ".into(),
            ..Default::default()
        };

        assert!(!draft.add_manual_model());
        assert!(draft.models.is_empty());
    }

    #[test]
    fn changing_protocol_moves_a_default_url_with_it() {
        // The old default would be silently wrong for the new protocol.
        let mut draft = ProviderDraft::default();
        draft.set_protocol(ProviderProtocol::AnthropicMessages);

        assert_eq!(
            draft.base_url,
            ProviderProtocol::AnthropicMessages.default_base_url()
        );
    }

    #[test]
    fn changing_protocol_leaves_a_typed_url_alone() {
        let mut draft = ProviderDraft {
            base_url: "http://localhost:8080/v1".into(),
            ..Default::default()
        };
        draft.set_protocol(ProviderProtocol::AnthropicMessages);

        assert_eq!(draft.base_url, "http://localhost:8080/v1");
        assert_eq!(draft.protocol, ProviderProtocol::AnthropicMessages);
    }

    #[test]
    fn a_search_engine_needs_a_name_and_a_url() {
        assert!(!SearchEngineDraft::default().can_save());
        assert!(
            SearchEngineDraft {
                base_url: "https://search.example".into(),
                ..Default::default()
            }
            .can_save()
        );
    }

    #[test]
    fn doubao_defaults_to_the_global_endpoint_and_requires_a_key() {
        let mut draft = SearchEngineDraft::default();
        draft.set_kind(SearchEngineKind::DoubaoGlobal);

        assert_eq!(draft.name, SearchEngineKind::DoubaoGlobal.default_name());
        assert_eq!(
            draft.base_url,
            "https://open.feedcoopapi.com/search_api/global_search"
        );
        assert!(!draft.can_save());

        draft.api_key = "  secret  ".into();
        assert_eq!(draft.typed_api_key().as_deref(), Some("secret"));
        assert!(draft.can_save());
        assert!(draft.to_profile(0).has_api_key);
    }

    #[test]
    fn changing_back_to_searxng_drops_draft_key_metadata() {
        let mut draft = SearchEngineDraft::default();
        draft.set_kind(SearchEngineKind::DoubaoGlobal);
        draft.api_key = "secret".into();
        draft.has_api_key = true;

        draft.set_kind(SearchEngineKind::Searxng);

        assert_eq!(draft.name, "SearXNG");
        assert!(draft.base_url.is_empty());
        assert!(draft.api_key.is_empty());
        assert!(!draft.has_api_key);
    }

    #[test]
    fn saving_a_new_engine_appends_it() {
        let existing = vec![engine("a", 0)];
        let draft = SearchEngineDraft {
            base_url: "https://b.example".into(),
            ..Default::default()
        };

        let saved = upsert_search_engine(&existing, &draft);

        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].base_url, "https://b.example");
        assert_eq!(saved[1].position, 1);
    }

    #[test]
    fn saving_an_edited_engine_replaces_it_in_place() {
        // Appending instead would duplicate the row and change the search order.
        let existing = vec![engine("a", 0), engine("b", 1)];
        let mut draft = SearchEngineDraft::from_profile(&existing[0]);
        draft.base_url = "https://moved.example".into();

        let saved = upsert_search_engine(&existing, &draft);

        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].base_url, "https://moved.example");
        assert_eq!(saved[0].id, existing[0].id);
    }

    #[test]
    fn removing_an_engine_renumbers_the_rest() {
        // Position decides which engine answers first, so a gap would misorder
        // the list on reload.
        let existing = vec![engine("a", 0), engine("b", 1), engine("c", 2)];

        let saved = remove_search_engine(&existing, 0);

        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].name, "b");
        assert_eq!(saved[0].position, 0);
        assert_eq!(saved[1].position, 1);
    }

    #[test]
    fn removing_past_the_end_changes_nothing() {
        let existing = vec![engine("a", 0)];

        assert_eq!(remove_search_engine(&existing, 5).len(), 1);
    }

    #[test]
    fn moving_an_engine_swaps_it_with_its_neighbour() {
        let existing = vec![engine("a", 0), engine("b", 1)];

        let saved = move_search_engine(&existing, 0, 1);

        assert_eq!(saved[0].name, "b");
        assert_eq!(saved[1].name, "a");
        assert_eq!(saved[0].position, 0);
        assert_eq!(saved[1].position, 1);
    }

    #[test]
    fn moving_past_either_end_does_nothing() {
        // Wrapping from top to bottom under one click is hard to aim.
        let existing = vec![engine("a", 0), engine("b", 1)];

        assert_eq!(move_search_engine(&existing, 0, -1)[0].name, "a");
        assert_eq!(move_search_engine(&existing, 1, 1)[1].name, "b");
    }

    #[test]
    fn the_sections_are_listed_in_a_deliberate_order() {
        // Local execution behavior belongs before connection-specific pages.
        assert_eq!(
            Section::ALL,
            [
                Section::General,
                Section::Execution,
                Section::Providers,
                Section::WebSearch
            ]
        );
    }
}
