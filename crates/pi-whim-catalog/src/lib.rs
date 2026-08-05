//! Model capability lookup, online and bundled.
//!
//! Pi-Whim needs to know what a model supports — reasoning, images, thinking
//! levels — to render the right controls and pass the right flags to Pi. Two
//! sources answer that: a catalog fetched from models.dev, and the table
//! compiled into `pi-whim-core` at build time from the vendored Pi checkout.
//!
//! The online catalog is a few megabytes, so it is fetched once on a background
//! thread and shared. Callers hold a [`ModelCapabilityResolver`]; lookups fall
//! back to the bundled table while the fetch is in flight or if it fails, so
//! this is never on a critical path.

use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
    time::Duration,
};

use crossbeam_channel::{Receiver, bounded};
use pi_whim_core::{
    CapabilityMatch, CatalogModelCapability, ModelCapability, ModelCapabilitySource, ProviderModel,
    ProviderProfile, ThinkingLevel, ThinkingLevelMap, resolve_bundled_capability,
    resolve_bundled_capability_by_model_id, resolve_catalog_capability,
};
use serde_json::Value;

const MODELS_DEV_CATALOG_URL: &str = "https://models.dev/api.json";

#[derive(Clone, Debug, Default)]
enum OnlineCatalogState {
    #[default]
    Empty,
    Loading,
    Ready(Arc<Vec<CatalogModelCapability>>),
    Unavailable,
}

/// A fetched catalog, shared by every resolver cloned from the same handle.
///
/// Held explicitly rather than in a process-global static, so tests and any
/// future second workspace get their own catalog instead of racing over one.
#[derive(Clone, Default)]
pub struct SharedCatalog {
    state: Arc<RwLock<OnlineCatalogState>>,
}

pub struct ModelCapabilityResolver {
    state: Arc<RwLock<OnlineCatalogState>>,
    completed: Receiver<()>,
}

impl Default for ModelCapabilityResolver {
    fn default() -> Self {
        Self::new(&SharedCatalog::default(), true)
    }
}

impl ModelCapabilityResolver {
    /// Build a resolver over `catalog`, fetching the online catalog into it if
    /// `fetch_online` is set and no other resolver has already started.
    pub fn new(catalog: &SharedCatalog, fetch_online: bool) -> Self {
        let state = catalog.state.clone();
        let (sender, completed) = bounded(1);
        if fetch_online {
            let should_fetch = state
                .write()
                .ok()
                .is_some_and(|mut catalog| match *catalog {
                    OnlineCatalogState::Empty => {
                        *catalog = OnlineCatalogState::Loading;
                        true
                    }
                    _ => false,
                });
            if should_fetch {
                let state = state.clone();
                std::thread::spawn(move || {
                    let result = fetch_online_catalog();
                    if let Ok(mut catalog) = state.write() {
                        *catalog = match result {
                            Ok(records) => OnlineCatalogState::Ready(Arc::new(records)),
                            Err(_) => OnlineCatalogState::Unavailable,
                        };
                    }
                    let _ = sender.send(());
                });
            }
        }
        Self { state, completed }
    }

    /// A channel that yields once, when the online fetch has finished.
    ///
    /// Handed out rather than polled: the caller has no frame to poll from, and a
    /// receiver is something it can block on off the main thread. Disconnected
    /// straight away when this resolver did not start a fetch, which reads as
    /// "there will be no refresh" — the same answer polling gave.
    pub fn refreshed(&self) -> Receiver<()> {
        self.completed.clone()
    }

    pub fn enrich_profile(&self, profile: &mut ProviderProfile) {
        let provider_ids = provider_catalog_ids(profile);
        for model in &mut profile.models {
            self.enrich_model(&provider_ids, model);
        }
    }

    pub fn enrich_models(&self, provider_name: &str, base_url: &str, models: &mut [ProviderModel]) {
        let provider_ids = provider_catalog_ids_from_parts(provider_name, base_url);
        for model in models {
            self.enrich_model(&provider_ids, model);
        }
    }

    fn enrich_model(&self, provider_ids: &[String], model: &mut ProviderModel) {
        let online_match = self.state.read().ok().and_then(|state| match &*state {
            OnlineCatalogState::Ready(catalog) => {
                Some(resolve_catalog_capability(catalog, provider_ids, &model.id))
            }
            _ => None,
        });
        match online_match {
            Some(CapabilityMatch::Found(capability)) => {
                model.apply_capability(capability);
                return;
            }
            Some(CapabilityMatch::Ambiguous) => {
                mark_unverified(model);
                return;
            }
            Some(CapabilityMatch::NotFound) | None => {}
        }
        match resolve_bundled_capability(provider_ids, &model.id) {
            CapabilityMatch::Found(capability) => model.apply_capability(capability),
            CapabilityMatch::Ambiguous => mark_unverified(model),
            CapabilityMatch::NotFound => {
                // A compatible gateway normally reports only the upstream model ID.
                // Prefer an unambiguous maker inferred from that stable ID before
                // considering a cross-provider fallback with weaker guarantees.
                match resolve_bundled_capability(&model_vendor_ids(&model.id), &model.id) {
                    CapabilityMatch::Found(capability) => model.apply_capability(capability),
                    CapabilityMatch::Ambiguous => mark_unverified(model),
                    CapabilityMatch::NotFound => {
                        match resolve_bundled_capability_by_model_id(&model.id) {
                            CapabilityMatch::Found(capability) => {
                                model.apply_capability(capability)
                            }
                            CapabilityMatch::NotFound | CapabilityMatch::Ambiguous => {
                                mark_unverified(model)
                            }
                        }
                    }
                }
            }
        }
    }
}

fn mark_unverified(model: &mut ProviderModel) {
    model.reasoning = false;
    model.thinking_level_map = ThinkingLevelMap::default();
    model.capability_source = ModelCapabilitySource::Unverified;
}

fn provider_catalog_ids(profile: &ProviderProfile) -> Vec<String> {
    provider_catalog_ids_from_parts(&profile.name, &profile.base_url)
}

fn provider_catalog_ids_from_parts(provider_name: &str, base_url: &str) -> Vec<String> {
    let mut ids = BTreeSet::from([provider_name.trim().to_owned()]);
    ids.extend(
        provider_name
            .split(|character: char| !character.is_alphanumeric() && character != '-')
            .map(str::trim)
            .filter(|part| part.chars().count() >= 3)
            .map(str::to_owned),
    );
    let base_url = base_url.to_ascii_lowercase();
    for (host, provider) in [
        ("api.openai.com", "openai"),
        ("api.anthropic.com", "anthropic"),
        ("generativelanguage.googleapis.com", "google"),
        ("openrouter.ai", "openrouter"),
        ("api.deepseek.com", "deepseek"),
        ("dashscope.aliyuncs.com", "alibaba"),
        ("api.mistral.ai", "mistral"),
        ("api.x.ai", "xai"),
    ] {
        if base_url.contains(host) {
            ids.insert(provider.to_owned());
        }
    }
    ids.into_iter().filter(|id| !id.is_empty()).collect()
}

/// Names used by model publishers in the Pi catalog. This only runs after a
/// configured provider/URL failed to identify itself, so it supports generic
/// OpenAI-compatible gateways without replacing an explicit provider match.
fn model_vendor_ids(model_id: &str) -> Vec<String> {
    let id = model_id.trim().to_ascii_lowercase();
    let vendor = if id.starts_with("claude") {
        Some("anthropic")
    } else if id.starts_with("deepseek") {
        Some("deepseek")
    } else if id.starts_with("gemini") {
        Some("google")
    } else if id.starts_with("gpt-")
        || matches!(
            id.as_str(),
            "o1" | "o1-mini" | "o1-pro" | "o3" | "o3-mini" | "o3-pro" | "o4-mini"
        )
    {
        Some("openai")
    } else if id.starts_with("glm-") {
        Some("zai")
    } else if id.starts_with("kimi-") {
        Some("moonshotai")
    } else {
        None
    };
    vendor.into_iter().map(str::to_owned).collect()
}

fn fetch_online_catalog() -> Result<Vec<CatalogModelCapability>, String> {
    let agent = ureq::Agent::config_builder()
        // The catalog is a few megabytes. Keep this async and give slower
        // connections enough time to complete instead of permanently using
        // the offline fallback after a partial download.
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .new_agent();
    let mut response = agent
        .get(MODELS_DEV_CATALOG_URL)
        .header("Accept", "application/json")
        .call()
        .map_err(|error| error.to_string())?;
    let value = response
        .body_mut()
        .read_json::<Value>()
        .map_err(|error| error.to_string())?;
    parse_online_catalog(&value)
}

fn parse_online_catalog(value: &Value) -> Result<Vec<CatalogModelCapability>, String> {
    let providers = value
        .as_object()
        .ok_or_else(|| "models.dev returned a non-object catalog".to_owned())?;
    let mut records = Vec::new();
    for (provider_id, provider) in providers {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (fallback_id, model) in models {
            let id = model
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(fallback_id);
            let name = model
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_owned();
            let reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let supports_images = model
                .get("attachment")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || model
                    .pointer("/modalities/input")
                    .and_then(Value::as_array)
                    .is_some_and(|inputs| {
                        inputs.iter().any(|input| input.as_str() == Some("image"))
                    });
            let thinking_level_map = online_thinking_level_map(model, reasoning);
            let max_output_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .map(|tokens| tokens as u32);
            records.push(CatalogModelCapability {
                provider: provider_id.clone(),
                id: id.to_owned(),
                capability: ModelCapability {
                    name,
                    reasoning,
                    supports_images,
                    thinking_level_map,
                    source: ModelCapabilitySource::OnlineCatalog,
                    recommended_protocol: None,
                    context_window: None,
                    max_output_tokens,
                },
            });
        }
    }
    Ok(records)
}

fn online_thinking_level_map(model: &Value, reasoning: bool) -> ThinkingLevelMap {
    if !reasoning {
        return ThinkingLevelMap::default();
    }
    let effort_values = model
        .get("reasoning_options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
        .flat_map(|option| {
            option
                .get("values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(Value::as_str)
        .filter_map(|level| ThinkingLevel::try_from(level).ok())
        .collect::<BTreeSet<_>>();
    if effort_values.is_empty() {
        return ThinkingLevelMap::default();
    }
    ThinkingLevelMap::from_entries(
        ThinkingLevel::ALL
            .into_iter()
            .filter(|level| *level != ThinkingLevel::Off)
            .map(|level| {
                let mapping = effort_values
                    .contains(&level)
                    .then(|| level.as_str().to_owned());
                (level, mapping)
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn online_catalog_maps_exact_effort_levels() {
        let catalog = parse_online_catalog(&json!({
            "openai": {
                "models": {
                    "gpt-example": {
                        "id": "gpt-example",
                        "name": "GPT Example",
                        "reasoning": true,
                        "attachment": true,
                        "reasoning_options": [{"type":"effort", "values":["low", "high", "xhigh"]}]
                    }
                }
            }
        }))
        .unwrap();
        let CapabilityMatch::Found(capability) =
            resolve_catalog_capability(&catalog, &["OpenAI".into()], "gpt-example")
        else {
            panic!("expected exact online match");
        };
        assert_eq!(
            capability.thinking_level_map.available_levels(true),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
            ]
        );
        assert!(capability.supports_images);
        assert_eq!(capability.source, ModelCapabilitySource::OnlineCatalog);
    }

    #[test]
    fn conflicting_exact_catalog_entries_are_ambiguous() {
        let capability = |reasoning| CatalogModelCapability {
            provider: "openai".into(),
            id: "same-id".into(),
            capability: ModelCapability {
                name: "Same".into(),
                reasoning,
                supports_images: false,
                thinking_level_map: ThinkingLevelMap::default(),
                source: ModelCapabilitySource::OnlineCatalog,
                recommended_protocol: None,
                context_window: None,
                max_output_tokens: None,
            },
        };
        assert_eq!(
            resolve_catalog_capability(
                &[capability(true), capability(false)],
                &["openai".into()],
                "same-id"
            ),
            CapabilityMatch::Ambiguous
        );
    }

    #[test]
    fn generic_gateway_uses_model_publisher_when_the_id_is_unambiguous() {
        let mut model = ProviderModel::new("deepseek-v4-pro");
        ModelCapabilityResolver::new(&SharedCatalog::default(), false).enrich_models(
            "OpenAI-compatible",
            "https://proxy.example/v1",
            std::slice::from_mut(&mut model),
        );
        assert!(model.reasoning);
        assert_eq!(
            model.available_thinking_levels(),
            vec![ThinkingLevel::Off, ThinkingLevel::High, ThinkingLevel::Max]
        );
        assert_eq!(
            model.capability_source,
            ModelCapabilitySource::BundledCatalog
        );
    }
}
