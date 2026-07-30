use pi_whim_core::{ProviderModel, ProviderProtocol, ThinkingLevel};
use serde_json::{Value, json};
use std::time::Duration;

use crate::{OneShotErrorKind, service::ProviderRuntime};

pub(crate) const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

pub(crate) fn execute(
    agent: &ureq::Agent,
    provider: &ProviderRuntime,
    system_prompt: &str,
    input: &str,
    max_output_tokens: u32,
    timeout: Duration,
) -> Result<String, OneShotErrorKind> {
    let endpoint = endpoint(&provider.base_url, provider.protocol, &provider.model.id)?;
    let body = request_body(provider, system_prompt, input, max_output_tokens);
    let mut request = agent.post(&endpoint).header("Accept", "application/json");
    request = match provider.protocol {
        ProviderProtocol::OpenAiCompletions | ProviderProtocol::OpenAiResponses => request.header(
            "Authorization",
            &format!("Bearer {}", provider.api_key.expose()),
        ),
        ProviderProtocol::AnthropicMessages => request
            .header("x-api-key", provider.api_key.expose())
            .header("anthropic-version", "2023-06-01"),
        ProviderProtocol::GoogleGenerativeAi => {
            request.header("x-goog-api-key", provider.api_key.expose())
        }
    };
    let mut response = request
        .config()
        .timeout_global(Some(timeout))
        .build()
        .send_json(&body)
        .map_err(classify_transport_error)?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES + 1)
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => OneShotErrorKind::ResponseTooLarge,
            other => classify_transport_error(other),
        })?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(OneShotErrorKind::ResponseTooLarge);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| OneShotErrorKind::InvalidResponse)?;
    extract_text(provider.protocol, &value)
        .filter(|text| !text.trim().is_empty())
        .ok_or(OneShotErrorKind::InvalidResponse)
}

fn endpoint(
    base_url: &str,
    protocol: ProviderProtocol,
    model_id: &str,
) -> Result<String, OneShotErrorKind> {
    let suffix = match protocol {
        ProviderProtocol::OpenAiCompletions => "chat/completions".to_owned(),
        ProviderProtocol::OpenAiResponses => "responses".to_owned(),
        ProviderProtocol::AnthropicMessages => "v1/messages".to_owned(),
        ProviderProtocol::GoogleGenerativeAi => {
            let encoded: String =
                url::form_urlencoded::byte_serialize(model_id.as_bytes()).collect();
            format!("models/{encoded}:generateContent")
        }
    };
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(OneShotErrorKind::InvalidConfiguration);
    }
    let suffix = if base.ends_with("/v1") && suffix.starts_with("v1/") {
        suffix.trim_start_matches("v1/")
    } else {
        &suffix
    };
    Ok(format!("{base}/{suffix}"))
}

fn request_body(
    provider: &ProviderRuntime,
    system_prompt: &str,
    input: &str,
    max_output_tokens: u32,
) -> Value {
    let effort = mapped_effort(&provider.model, provider.thinking_level);
    match provider.protocol {
        ProviderProtocol::OpenAiCompletions => {
            let mut body = json!({
                "model": provider.model.id,
                "messages": [
                    {"role": "system", "content": system_prompt},
                    {"role": "user", "content": input}
                ],
                "max_tokens": max_output_tokens,
                "stream": false
            });
            if let Some(effort) = effort {
                body["reasoning_effort"] = json!(effort);
            }
            body
        }
        ProviderProtocol::OpenAiResponses => {
            let mut body = json!({
                "model": provider.model.id,
                "instructions": system_prompt,
                "input": input,
                "max_output_tokens": max_output_tokens,
                "stream": false,
                "store": false
            });
            if let Some(effort) = effort {
                body["reasoning"] = json!({"effort": effort});
            }
            body
        }
        ProviderProtocol::AnthropicMessages => {
            let mut body = json!({
                "model": provider.model.id,
                "system": system_prompt,
                "messages": [{"role": "user", "content": input}],
                "max_tokens": max_output_tokens,
                "stream": false
            });
            if let Some(effort) = effort {
                body["thinking"] = json!({"type": "adaptive"});
                body["output_config"] = json!({"effort": effort});
            }
            body
        }
        ProviderProtocol::GoogleGenerativeAi => {
            let mut config = json!({"maxOutputTokens": max_output_tokens});
            if let Some(effort) = effort {
                config["thinkingConfig"] = json!({
                    "includeThoughts": false,
                    "thinkingLevel": google_effort(&effort)
                });
            } else if provider.model.reasoning {
                config["thinkingConfig"] = google_disabled_thinking(&provider.model.id);
            }
            json!({
                "systemInstruction": {"parts": [{"text": system_prompt}]},
                "contents": [{"role": "user", "parts": [{"text": input}]}],
                "generationConfig": config
            })
        }
    }
}

fn mapped_effort(model: &ProviderModel, level: ThinkingLevel) -> Option<String> {
    match model.thinking_level_map.0.get(&level) {
        Some(Some(mapped)) => Some(mapped.clone()),
        Some(None) => None,
        None if level == ThinkingLevel::Off => None,
        None => Some(level.as_str().to_owned()),
    }
}

fn google_effort(effort: &str) -> String {
    match effort.to_ascii_lowercase().as_str() {
        "minimal" => "MINIMAL",
        "low" => "LOW",
        "medium" => "MEDIUM",
        "high" | "xhigh" | "max" => "HIGH",
        other => other,
    }
    .to_owned()
}

fn google_disabled_thinking(model_id: &str) -> Value {
    let id = model_id.to_ascii_lowercase();
    if id.contains("gemini-3") && id.contains("pro") {
        json!({"thinkingLevel": "LOW"})
    } else if id.contains("gemini-3") || id.contains("gemma-4") || id.contains("gemma4") {
        json!({"thinkingLevel": "MINIMAL"})
    } else {
        json!({"thinkingBudget": 0})
    }
}

fn extract_text(protocol: ProviderProtocol, body: &Value) -> Option<String> {
    match protocol {
        ProviderProtocol::OpenAiCompletions => {
            let content = body.pointer("/choices/0/message/content")?;
            if let Some(text) = content.as_str() {
                return Some(text.to_owned());
            }
            content.as_array().map(|parts| text_parts(parts, false))
        }
        ProviderProtocol::OpenAiResponses => body
            .get("output_text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                let mut text = String::new();
                for content in body
                    .get("output")?
                    .as_array()?
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                    .filter_map(|item| item.get("content").and_then(Value::as_array))
                {
                    text.push_str(&text_parts(content, false));
                }
                Some(text)
            }),
        ProviderProtocol::AnthropicMessages => body
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| text_parts(parts, false)),
        ProviderProtocol::GoogleGenerativeAi => body
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .map(|parts| text_parts(parts, true)),
    }
}

fn text_parts(parts: &[Value], skip_thoughts: bool) -> String {
    parts
        .iter()
        .filter(|part| !skip_thoughts || part.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect()
}

fn classify_transport_error(error: ureq::Error) -> OneShotErrorKind {
    match error {
        ureq::Error::Timeout(_) => OneShotErrorKind::TimedOut,
        ureq::Error::StatusCode(401 | 403) => OneShotErrorKind::Unauthorized,
        ureq::Error::StatusCode(429) => OneShotErrorKind::RateLimited,
        ureq::Error::StatusCode(_) => OneShotErrorKind::ProviderRejected,
        ureq::Error::BodyExceedsLimit(_) => OneShotErrorKind::ResponseTooLarge,
        _ => OneShotErrorKind::Network,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(protocol: ProviderProtocol) -> ProviderRuntime {
        ProviderRuntime {
            base_url: "https://api.example.test/v1".into(),
            protocol,
            model: ProviderModel::new("model/name"),
            thinking_level: ThinkingLevel::Off,
            api_key: crate::service::Secret::new("secret".into()),
        }
    }

    #[test]
    fn all_protocol_requests_are_non_streaming_and_tool_free() {
        for protocol in ProviderProtocol::ALL {
            let provider = provider(protocol);
            let endpoint = endpoint(&provider.base_url, protocol, &provider.model.id).unwrap();
            let body = request_body(&provider, "system", "only this text", 128);
            let encoded = body.to_string();

            assert!(!encoded.contains("tools"));
            assert!(!encoded.contains("functions"));
            assert!(!encoded.contains("session"));
            assert!(encoded.contains("only this text"));
            assert!(encoded.contains("128"));
            match protocol {
                ProviderProtocol::OpenAiCompletions => {
                    assert!(endpoint.ends_with("/chat/completions"));
                    assert_eq!(body["stream"], false);
                }
                ProviderProtocol::OpenAiResponses => {
                    assert!(endpoint.ends_with("/responses"));
                    assert_eq!(body["stream"], false);
                    assert_eq!(body["store"], false);
                }
                ProviderProtocol::AnthropicMessages => {
                    assert!(endpoint.ends_with("/v1/messages"));
                    assert_eq!(body["stream"], false);
                }
                ProviderProtocol::GoogleGenerativeAi => {
                    assert!(endpoint.ends_with("/models/model%2Fname:generateContent"));
                }
            }
        }
    }

    #[test]
    fn response_extractors_ignore_reasoning() {
        assert_eq!(
            extract_text(
                ProviderProtocol::OpenAiResponses,
                &json!({
                    "output": [
                        {"type":"reasoning", "content":[{"type":"reasoning_text","text":"secret"}]},
                        {"type":"message", "content":[{"type":"output_text","text":"Title"}]}
                    ]
                })
            ),
            Some("Title".into())
        );
        assert_eq!(
            extract_text(
                ProviderProtocol::GoogleGenerativeAi,
                &json!({
                    "candidates":[{"content":{"parts":[
                        {"thought":true,"text":"reasoning"}, {"text":"标题"}
                    ]}}]
                })
            ),
            Some("标题".into())
        );
    }
}
