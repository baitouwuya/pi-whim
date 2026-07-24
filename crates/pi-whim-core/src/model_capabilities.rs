use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

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

impl ModelCapabilitySource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::OnlineCatalog => "models.dev",
            Self::BundledCatalog => "Pi catalog",
            Self::Unverified => "Unverified",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapability {
    pub name: String,
    pub reasoning: bool,
    pub supports_images: bool,
    pub thinking_level_map: ThinkingLevelMap,
    pub source: ModelCapabilitySource,
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
    }
}

fn same_usable_capability(left: &ModelCapability, right: &ModelCapability) -> bool {
    left.reasoning == right.reasoning
        && left.supports_images == right.supports_images
        && left.thinking_level_map == right.thinking_level_map
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
}
