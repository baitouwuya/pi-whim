//! Provider configuration and model discovery.
//!
//! Pi-Whim keeps provider credentials in the keychain and hands Pi a generated
//! `models.json` that references them by environment variable name, never by
//! value. These helpers build that config, probe provider endpoints to discover
//! available models, and validate search-engine URLs.

use pi_whim_core::{
    ProviderId, ProviderModel, ProviderProfile, ProviderProtocol, SearchEngineId,
    SearchEngineProfile,
};
use pi_whim_runtime::SearchEngineApiKeys;
use serde_json::{Value, json};
use std::collections::HashMap;

pub fn provider_keychain_account(id: ProviderId) -> String {
    format!("provider-api-key-{id}")
}

pub fn search_engine_keychain_account(id: SearchEngineId) -> String {
    format!("search-engine-api-key-{id}")
}

pub fn configured_search_engine_api_keys(
    profiles: &[SearchEngineProfile],
    mut get_key: impl FnMut(SearchEngineId) -> Result<Option<String>, String>,
) -> Result<SearchEngineApiKeys, String> {
    let mut api_keys = SearchEngineApiKeys::default();
    for profile in profiles
        .iter()
        .filter(|profile| profile.kind.requires_api_key())
    {
        if let Some(api_key) = get_key(profile.id)?
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            api_keys.insert(profile.id, api_key);
        }
    }
    Ok(api_keys)
}

fn provider_environment_name(id: ProviderId) -> String {
    format!("PI_WHIM_PROVIDER_{}", id.simple())
}

pub(crate) fn configured_provider_environment(
    profiles: Vec<ProviderProfile>,
    mut get_key: impl FnMut(ProviderId) -> Result<Option<String>, String>,
) -> Result<(Vec<ProviderProfile>, HashMap<String, String>), String> {
    let had_profiles = !profiles.is_empty();
    let mut configured_profiles = Vec::new();
    let mut environment = HashMap::new();
    for profile in profiles {
        if let Some(key) = get_key(profile.id)? {
            environment.insert(provider_environment_name(profile.id), key);
            configured_profiles.push(profile);
        }
    }
    if had_profiles && configured_profiles.is_empty() {
        return Err(
            "No configured provider has an API key in Keychain. Open Settings > Providers, select a provider, and save its API key."
                .into(),
        );
    }
    Ok((configured_profiles, environment))
}

pub fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_owned()
}

pub fn valid_search_engine_url(value: &str) -> bool {
    let value = normalize_base_url(value);
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https") && !rest.is_empty() && !rest.starts_with('/')
}

pub(crate) fn pi_models_json(profiles: &[ProviderProfile]) -> Value {
    let providers = profiles
        .iter()
        .map(|profile| {
            let models = profile
                .models
                .iter()
                .map(|model| {
                    json!({
                        "id": model.id,
                        "name": model.name,
                        "reasoning": model.reasoning,
                        "thinkingLevelMap": model.thinking_level_map,
                        "input": if model.supports_images { json!(["text", "image"]) } else { json!(["text"]) },
                        "contextWindow": 128000,
                        "maxTokens": 16384,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                    })
                })
                .collect::<Vec<_>>();
            (
                provider_config_key(profile.id),
                json!({
                    "name": profile.name,
                    "baseUrl": normalize_base_url(&profile.base_url),
                    "api": profile.protocol.pi_api(),
                    "apiKey": format!("${}", provider_environment_name(profile.id)),
                    "models": models,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({ "providers": providers })
}

pub fn provider_config_key(id: ProviderId) -> String {
    format!("pi-whim-{}", id.simple())
}

pub fn discover_models(
    base_url: &str,
    protocol: ProviderProtocol,
    api_key: Option<&str>,
) -> Result<Vec<ProviderModel>, String> {
    let base_url = normalize_base_url(base_url);
    if base_url.is_empty() {
        return Err("Enter a base URL before discovering models.".into());
    }
    let endpoint = match protocol {
        ProviderProtocol::OpenAiCompletions | ProviderProtocol::OpenAiResponses => {
            join_api_path(&base_url, "models")
        }
        ProviderProtocol::AnthropicMessages => join_api_path(&base_url, "v1/models"),
        ProviderProtocol::GoogleGenerativeAi => join_api_path(&base_url, "models"),
    };
    let mut request = ureq::get(&endpoint).header("Accept", "application/json");
    if let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = match protocol {
            ProviderProtocol::OpenAiCompletions | ProviderProtocol::OpenAiResponses => {
                request.header("Authorization", &format!("Bearer {api_key}"))
            }
            ProviderProtocol::AnthropicMessages => request
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01"),
            ProviderProtocol::GoogleGenerativeAi => request.header("x-goog-api-key", api_key),
        };
    }
    let mut response = request
        .call()
        .map_err(|error| format!("Model discovery failed: {error}"))?;
    let body: Value = response
        .body_mut()
        .read_json()
        .map_err(|error| format!("Model discovery returned invalid JSON: {error}"))?;
    let candidates = match protocol {
        ProviderProtocol::OpenAiCompletions | ProviderProtocol::OpenAiResponses => body
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry.get("id").and_then(Value::as_str).map(|id| {
                    let mut model = ProviderModel::new(id);
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
        ProviderProtocol::AnthropicMessages => body
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry.get("id").and_then(Value::as_str).map(|id| {
                    let mut model = ProviderModel::new(id);
                    model.name = entry
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned();
                    model
                })
            })
            .collect(),
        ProviderProtocol::GoogleGenerativeAi => body
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry.get("name").and_then(Value::as_str).map(|id| {
                    let id = id.strip_prefix("models/").unwrap_or(id);
                    let mut model = ProviderModel::new(id);
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
    };
    let mut models: Vec<ProviderModel> = candidates;
    models.sort_by_key(|model| model.name.to_lowercase());
    models.dedup_by(|left, right| left.id == right.id);
    Ok(models)
}

pub fn join_api_path(base_url: &str, suffix: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let suffix = suffix.trim_start_matches('/');
    if base_url.ends_with("/v1") && suffix.starts_with("v1/") {
        format!("{base_url}/{}", suffix.trim_start_matches("v1/"))
    } else {
        format!("{base_url}/{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{ModelCapabilitySource, ThinkingLevel, ThinkingLevelMap};
    use uuid::Uuid;

    #[test]
    fn generated_pi_models_config_only_references_a_key_environment_variable() {
        let mut model = ProviderModel::new("gpt-example");
        model.reasoning = true;
        model.thinking_level_map = ThinkingLevelMap::from_entries([
            (ThinkingLevel::Minimal, None),
            (ThinkingLevel::Xhigh, Some("xhigh".into())),
        ]);
        model.capability_source = ModelCapabilitySource::BundledCatalog;
        let profile = ProviderProfile {
            id: Uuid::new_v4(),
            name: "Private gateway".into(),
            base_url: "https://gateway.example/v1/".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![model],
            updated_at_ms: 1,
            has_api_key: true,
        };
        let config = pi_models_json(std::slice::from_ref(&profile));
        let provider = config["providers"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(provider["baseUrl"], "https://gateway.example/v1");
        assert_eq!(
            provider["apiKey"],
            format!("${}", provider_environment_name(profile.id))
        );
        assert!(!config.to_string().contains("sk-"));
        assert_eq!(provider["models"][0]["reasoning"], true);
        assert_eq!(
            provider["models"][0]["thinkingLevelMap"]["minimal"],
            Value::Null
        );
        assert_eq!(provider["models"][0]["thinkingLevelMap"]["xhigh"], "xhigh");
    }

    #[test]
    fn provider_without_a_key_does_not_block_a_configured_provider() {
        let missing_id = Uuid::new_v4();
        let configured_id = Uuid::new_v4();
        let profile = |id| ProviderProfile {
            id,
            name: "Private gateway".into(),
            base_url: "https://gateway.example/v1".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("gpt-example")],
            updated_at_ms: 1,
            has_api_key: id == configured_id,
        };

        let (profiles, environment) = configured_provider_environment(
            vec![profile(missing_id), profile(configured_id)],
            |id| Ok((id == configured_id).then(|| "secret-key".to_owned())),
        )
        .unwrap();

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, configured_id);
        assert_eq!(
            environment.get(&provider_environment_name(configured_id)),
            Some(&"secret-key".to_owned())
        );
        assert!(!environment.contains_key(&provider_environment_name(missing_id)));
    }

    #[test]
    fn search_engine_urls_accept_local_http_and_secure_https_only() {
        assert!(valid_search_engine_url("http://localhost:8080"));
        assert!(valid_search_engine_url("https://search.example/"));
        assert!(!valid_search_engine_url("search.example"));
        assert!(!valid_search_engine_url("ftp://search.example"));
        assert!(!valid_search_engine_url("https:///search.example"));
    }

    #[test]
    fn search_engine_keys_use_a_separate_redacted_runtime_map() {
        let profile = SearchEngineProfile::new_doubao_global();
        let api_keys = configured_search_engine_api_keys(std::slice::from_ref(&profile), |id| {
            Ok((id == profile.id).then(|| "  secret-key  ".to_owned()))
        })
        .unwrap();

        assert_eq!(
            search_engine_keychain_account(profile.id),
            format!("search-engine-api-key-{}", profile.id)
        );
        assert_eq!(api_keys.len(), 1);
        assert!(!format!("{api_keys:?}").contains("secret-key"));
    }
}
