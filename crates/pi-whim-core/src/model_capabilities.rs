use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ProviderProtocol;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for ThinkingLevel {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThinkingLevelMap(pub BTreeMap<ThinkingLevel, Option<String>>);

impl ThinkingLevelMap {
    pub fn from_entries(
        entries: impl IntoIterator<Item = (ThinkingLevel, Option<String>)>,
    ) -> Self {
        Self(entries.into_iter().collect())
    }

    pub fn available_levels(&self, reasoning: bool) -> Vec<ThinkingLevel> {
        if !reasoning {
            return vec![ThinkingLevel::Off];
        }
        ThinkingLevel::ALL
            .into_iter()
            .filter(|level| match self.0.get(level) {
                Some(None) => false,
                Some(Some(_)) => true,
                None => !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max),
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapabilitySource {
    OnlineCatalog,
    BundledCatalog,
    #[default]
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapability {
    pub name: String,
    pub reasoning: bool,
    pub supports_images: bool,
    pub thinking_level_map: ThinkingLevelMap,
    pub source: ModelCapabilitySource,
    /// The protocol the vendor catalog recommends for this model.
    pub recommended_protocol: Option<ProviderProtocol>,
    /// Context window size in tokens, from the vendor catalog.
    pub context_window: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModelCapability {
    pub provider: String,
    pub id: String,
    pub capability: ModelCapability,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityMatch {
    Found(ModelCapability),
    NotFound,
    Ambiguous,
}

pub(super) struct BundledCapability {
    provider: &'static str,
    id: &'static str,
    name: &'static str,
    reasoning: bool,
    supports_images: bool,
    thinking_level_map: &'static [(&'static str, Option<&'static str>)],
    api: &'static str,
    context_window: Option<u32>,
}

include!(concat!(env!("OUT_DIR"), "/bundled_model_capabilities.rs"));

pub fn normalize_provider_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn normalize_provider_display_name(value: &str) -> String {
    value.trim().to_owned()
}

pub fn provider_name_key(value: &str) -> String {
    normalize_provider_display_name(value).to_lowercase()
}

pub fn resolve_bundled_capability(provider_ids: &[String], model_id: &str) -> CapabilityMatch {
    let records = BUNDLED_CAPABILITIES.iter().filter(|record| {
        record.id == model_id
            && provider_ids.iter().any(|provider| {
                normalize_provider_name(provider) == normalize_provider_name(record.provider)
            })
    });
    resolve_matches(records.map(bundled_capability))
}

/// Resolve a model for a compatible gateway when its provider cannot be named
/// reliably. A fallback is only safe when every Pi catalog entry for this ID
/// agrees on its usable capabilities; provider-specific differences stay
/// unverified rather than being guessed.
pub fn resolve_bundled_capability_by_model_id(model_id: &str) -> CapabilityMatch {
    let mut matches = BUNDLED_CAPABILITIES
        .iter()
        .filter(|record| record.id == model_id)
        .map(bundled_capability);
    let Some(first) = matches.next() else {
        return CapabilityMatch::NotFound;
    };
    if matches.any(|candidate| !same_usable_capability(&candidate, &first)) {
        CapabilityMatch::Ambiguous
    } else {
        CapabilityMatch::Found(first)
    }
}

pub fn resolve_catalog_capability(
    catalog: &[CatalogModelCapability],
    provider_ids: &[String],
    model_id: &str,
) -> CapabilityMatch {
    let matches = catalog.iter().filter(|record| {
        record.id == model_id
            && provider_ids.iter().any(|provider| {
                normalize_provider_name(provider) == normalize_provider_name(&record.provider)
            })
    });
    resolve_matches(matches.map(|record| record.capability.clone()))
}

fn resolve_matches(matches: impl Iterator<Item = ModelCapability>) -> CapabilityMatch {
    let mut matches = matches;
    let Some(first) = matches.next() else {
        return CapabilityMatch::NotFound;
    };
    if matches.any(|candidate| candidate != first) {
        CapabilityMatch::Ambiguous
    } else {
        CapabilityMatch::Found(first)
    }
}

fn bundled_capability(record: &BundledCapability) -> ModelCapability {
    ModelCapability {
        name: record.name.to_owned(),
        reasoning: record.reasoning,
        supports_images: record.supports_images,
        thinking_level_map: ThinkingLevelMap::from_entries(
            record
                .thinking_level_map
                .iter()
                .filter_map(|(level, mapped)| {
                    ThinkingLevel::try_from(*level)
                        .ok()
                        .map(|level| (level, mapped.map(str::to_owned)))
                }),
        ),
        source: ModelCapabilitySource::BundledCatalog,
        recommended_protocol: ProviderProtocol::from_pi_api(record.api),
        context_window: record.context_window,
    }
}

fn same_usable_capability(left: &ModelCapability, right: &ModelCapability) -> bool {
    left.reasoning == right.reasoning
        && left.supports_images == right.supports_images
        && left.thinking_level_map == right.thinking_level_map
        && left.recommended_protocol == right.recommended_protocol
        && left.context_window == right.context_window
}

/// Discovery shape for a custom Pi provider: how to reach its model-listing
/// endpoint, how to authenticate that request, and how to parse the response.
/// Centralising these per-protocol branches here means adding a new protocol
/// only touches this `impl` block, not every discovery call site.
impl crate::ProviderProtocol {
    /// Append the protocol's model-listing path to a normalized base URL.
    pub fn discover_endpoint(self, base_url: &str) -> String {
        let suffix = match self {
            Self::OpenAiCompletions | Self::OpenAiResponses => "models",
            Self::AnthropicMessages => "v1/models",
            Self::GoogleGenerativeAi => "models",
        };
        let base_url = base_url.trim_end_matches('/');
        let suffix = suffix.trim_start_matches('/');
        if base_url.ends_with("/v1") && suffix.starts_with("v1/") {
            format!("{base_url}/{}", suffix.trim_start_matches("v1/"))
        } else {
            format!("{base_url}/{suffix}")
        }
    }

    /// Auth headers a model-discovery request must carry for a non-empty key.
    pub fn discovery_auth_headers(self, api_key: &str) -> Vec<(&'static str, String)> {
        match self {
            Self::OpenAiCompletions | Self::OpenAiResponses => {
                vec![("Authorization", format!("Bearer {api_key}"))]
            }
            Self::AnthropicMessages => vec![
                ("x-api-key", api_key.to_owned()),
                ("anthropic-version", "2023-06-01".to_owned()),
            ],
            Self::GoogleGenerativeAi => vec![("x-goog-api-key", api_key.to_owned())],
        }
    }

    /// Parse the protocol's model-listing response into local ProviderModel records.
    pub fn parse_models(self, body: &Value) -> Vec<crate::ProviderModel> {
        match self {
            // OpenAI and Anthropic both list models under `data[].id` with an optional
            // `display_name`; Anthropic's response carries no `name` field, so the
            // fallback to `name` is inert for it but keeps the two branches aligned.
            Self::OpenAiCompletions | Self::OpenAiResponses | Self::AnthropicMessages => body
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| {
                    entry.get("id").and_then(Value::as_str).map(|id| {
                        let mut model = crate::ProviderModel::new(id);
                        model.name = entry
                            .get("display_name")
                            .or_else(|| entry.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_owned();
                        model
                    })
                })
                .collect(),
            Self::GoogleGenerativeAi => body
                .get("models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| {
                    entry.get("name").and_then(Value::as_str).map(|id| {
                        let id = id.strip_prefix("models/").unwrap_or(id);
                        let mut model = crate::ProviderModel::new(id);
                        model.name = entry
                            .get("displayName")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_owned();
                        model.supports_images = entry
                            .get("supportedGenerationMethods")
                            .and_then(Value::as_array)
                            .is_some_and(|methods| {
                                methods.iter().any(|method| method == "generateContent")
                            });
                        model
                    })
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_level_rules_require_explicit_extended_levels() {
        let default_map = ThinkingLevelMap::default();
        assert_eq!(
            default_map.available_levels(true),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ]
        );
        let mapped = ThinkingLevelMap::from_entries([
            (ThinkingLevel::Minimal, None),
            (ThinkingLevel::Xhigh, Some("xhigh".into())),
        ]);
        assert_eq!(
            mapped.available_levels(true),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
            ]
        );
    }

    #[test]
    fn bundled_catalog_resolves_exact_provider_and_model() {
        let result = resolve_bundled_capability(&["OpenAI".into()], "gpt-5.6-sol");
        let CapabilityMatch::Found(capability) = result else {
            panic!("expected bundled capability");
        };
        assert!(capability.reasoning);
        assert!(
            capability
                .thinking_level_map
                .available_levels(true)
                .contains(&ThinkingLevel::Max)
        );
        assert_eq!(capability.source, ModelCapabilitySource::BundledCatalog);
    }

    #[test]
    fn bundled_catalog_resolves_unambiguous_model_id_for_compatible_gateway() {
        let CapabilityMatch::Found(capability) =
            resolve_bundled_capability_by_model_id("claude-opus-4-7")
        else {
            panic!("expected compatible gateway fallback");
        };
        assert!(capability.reasoning);
        assert_eq!(
            capability.thinking_level_map.available_levels(true),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
                ThinkingLevel::Max,
            ]
        );
    }

    #[test]
    fn compatible_gateway_fallback_refuses_provider_dependent_capabilities() {
        assert_eq!(
            resolve_bundled_capability_by_model_id("gpt-5.6-sol"),
            CapabilityMatch::Ambiguous
        );
    }

    #[test]
    fn unknown_provider_does_not_guess_from_model_id() {
        assert_eq!(
            resolve_bundled_capability(&["Private proxy".into()], "gpt-5.6-sol"),
            CapabilityMatch::NotFound
        );
    }

    #[test]
    fn discover_endpoint_appends_per_protocol_path() {
        assert_eq!(
            crate::ProviderProtocol::OpenAiCompletions
                .discover_endpoint("https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            crate::ProviderProtocol::AnthropicMessages
                .discover_endpoint("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            crate::ProviderProtocol::GoogleGenerativeAi
                .discover_endpoint("https://generativelanguage.googleapis.com/v1beta"),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
    }

    #[test]
    fn discover_endpoint_avoids_double_v1_segment() {
        // base already ending in /v1 with a v1/ suffix must not produce /v1/v1/.
        assert_eq!(
            crate::ProviderProtocol::AnthropicMessages.discover_endpoint("https://x/v1"),
            "https://x/v1/models"
        );
    }

    #[test]
    fn discover_endpoint_trims_trailing_slash() {
        assert_eq!(
            crate::ProviderProtocol::OpenAiCompletions.discover_endpoint("https://x/v1/"),
            "https://x/v1/models"
        );
    }

    #[test]
    fn discovery_auth_headers_shape_per_protocol() {
        assert_eq!(
            crate::ProviderProtocol::OpenAiCompletions.discovery_auth_headers("sk-x"),
            vec![("Authorization", "Bearer sk-x".to_string())]
        );
        assert_eq!(
            crate::ProviderProtocol::AnthropicMessages.discovery_auth_headers("sk-a"),
            vec![
                ("x-api-key", "sk-a".to_string()),
                ("anthropic-version", "2023-06-01".to_string()),
            ]
        );
        assert_eq!(
            crate::ProviderProtocol::GoogleGenerativeAi.discovery_auth_headers("key-g"),
            vec![("x-goog-api-key", "key-g".to_string())]
        );
    }

    #[test]
    fn parse_models_openai_uses_display_name_then_name_fallback() {
        let body = serde_json::json!({
            "data": [
                {"id": "gpt-x", "display_name": "GPT X"},
                {"id": "plain", "name": "Plain Named"}
            ]
        });
        let mut models = crate::ProviderProtocol::OpenAiCompletions.parse_models(&body);
        models.sort_by_key(|model| model.id.clone());
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-x");
        assert_eq!(models[0].name, "GPT X");
        assert_eq!(models[1].id, "plain");
        assert_eq!(models[1].name, "Plain Named");
    }

    #[test]
    fn parse_models_anthropic_falls_back_to_id_when_display_name_absent() {
        // Anthropic responses carry no `name` field, so the name fallback is inert
        // and the model's display name is its id.
        let body = serde_json::json!({"data": [{"id": "claude-x"}]});
        let models = crate::ProviderProtocol::AnthropicMessages.parse_models(&body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-x");
        assert_eq!(models[0].name, "claude-x");
    }

    #[test]
    fn parse_models_google_strips_prefix_and_detects_image_support() {
        let body = serde_json::json!({
            "models": [
                {"name": "models/gemini-x", "displayName": "Gemini X", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/embedding-y", "displayName": "Embedding Y", "supportedGenerationMethods": ["embedContent"]}
            ]
        });
        let mut models = crate::ProviderProtocol::GoogleGenerativeAi.parse_models(&body);
        models.sort_by_key(|model| model.id.clone());
        assert_eq!(models[0].id, "embedding-y");
        assert!(!models[0].supports_images);
        assert_eq!(models[1].id, "gemini-x");
        assert!(models[1].supports_images);
    }

    #[test]
    fn parse_models_missing_or_non_array_field_returns_empty() {
        assert_eq!(
            crate::ProviderProtocol::OpenAiCompletions.parse_models(&serde_json::json!({})),
            Vec::<crate::ProviderModel>::new()
        );
        assert_eq!(
            crate::ProviderProtocol::GoogleGenerativeAi
                .parse_models(&serde_json::json!({"models": "not-an-array"})),
            Vec::<crate::ProviderModel>::new()
        );
    }
}
