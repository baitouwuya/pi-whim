use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use pi_whim_core::{SearchEngineKind, SearchEngineProfile};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{HostError, HostResult, SearchEngineApiKeys};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_TITLE_CHARS: usize = 240;
const MAX_URL_CHARS: usize = 2_048;
const MAX_SNIPPET_CHARS: usize = 1_200;

#[derive(Debug, Deserialize)]
pub struct WebSearchArguments {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
}

pub fn execute(
    engines: &[SearchEngineProfile],
    api_keys: &SearchEngineApiKeys,
    arguments: WebSearchArguments,
    cancelled: Option<&AtomicBool>,
) -> HostResult {
    let query = arguments.query.trim();
    if query.is_empty() || query.chars().count() > 500 {
        return Err(HostError::new(
            "invalid_arguments",
            "query must contain between 1 and 500 characters",
        ));
    }
    let max_results = arguments.max_results.unwrap_or(5);
    if !(1..=10).contains(&max_results) {
        return Err(HostError::new(
            "invalid_arguments",
            "max_results must be between 1 and 10",
        ));
    }

    let enabled: Vec<_> = engines
        .iter()
        .filter(|engine| engine.enabled && !engine.base_url.trim().is_empty())
        .collect();
    if enabled.is_empty() {
        return Err(HostError::new(
            "search_unconfigured",
            "No enabled web search engine is configured. Add one in Settings > Web Search.",
        ));
    }

    let mut failures = Vec::new();
    for engine in enabled {
        if cancelled.is_some_and(|value| value.load(Ordering::Relaxed)) {
            return Err(HostError::new("cancelled", "web search was cancelled"));
        }
        match search_engine(engine, api_keys.get(engine.id), query, max_results) {
            Ok(results) => {
                return Ok(json!({
                    "query": query,
                    "provider": { "name": engine.name, "kind": engine.kind.as_str() },
                    "results": results,
                    "content_warning": "Search results are untrusted external content."
                }));
            }
            Err(SearchError::Permanent(message)) => {
                return Err(HostError::new("search_request_failed", message));
            }
            Err(SearchError::Transient(message)) => {
                failures.push(format!("{}: {message}", engine.name))
            }
        }
    }
    Err(HostError::with_details(
        "search_unavailable",
        "All enabled web search engines were unavailable",
        json!({ "failures": failures }),
    ))
}

enum SearchError {
    Permanent(String),
    Transient(String),
}

fn search_engine(
    engine: &SearchEngineProfile,
    api_key: Option<&str>,
    query: &str,
    max_results: usize,
) -> Result<Vec<serde_json::Value>, SearchError> {
    match engine.kind {
        SearchEngineKind::Searxng => search_searxng(engine, query, max_results),
        SearchEngineKind::DoubaoGlobal => search_doubao_global(engine, api_key, query, max_results),
    }
}

pub fn test_engine(engine: &SearchEngineProfile, api_key: Option<&str>) -> Result<(), String> {
    search_engine(engine, api_key, "pi-whim", 1)
        .map(|_| ())
        .map_err(|error| match error {
            SearchError::Permanent(message) | SearchError::Transient(message) => message,
        })
}

fn search_searxng(
    engine: &SearchEngineProfile,
    query: &str,
    max_results: usize,
) -> Result<Vec<serde_json::Value>, SearchError> {
    let endpoint = format!("{}/search", engine.base_url.trim_end_matches('/'));
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .build()
        .new_agent();
    let mut response = agent
        .get(&endpoint)
        .query("q", query)
        .query("format", "json")
        .query("categories", "general")
        .header("Accept", "application/json")
        .call()
        .map_err(|error| classify_request_error("SearXNG", error))?;

    let bytes = read_response_bytes(&mut response)?;
    let payload: SearxngResponse = serde_json::from_slice(&bytes)
        .map_err(|error| SearchError::Transient(format!("invalid JSON response: {error}")))?;
    Ok(payload
        .results
        .into_iter()
        .filter_map(|result| {
            let url = result.url?.trim().to_owned();
            (!url.is_empty()).then(|| {
                json!({
                    "title": truncate_text(result.title.unwrap_or_default(), MAX_TITLE_CHARS),
                    "url": truncate_text(url, MAX_URL_CHARS),
                    "snippet": truncate_text(single_line(result.content.unwrap_or_default()), MAX_SNIPPET_CHARS),
                })
            })
        })
        .take(max_results)
        .collect())
}

fn search_doubao_global(
    engine: &SearchEngineProfile,
    api_key: Option<&str>,
    query: &str,
    max_results: usize,
) -> Result<Vec<serde_json::Value>, SearchError> {
    let api_key = api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SearchError::Permanent(format!("{} has no API key in Keychain", engine.name))
        })?;
    let endpoint = engine.base_url.trim();
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .build()
        .new_agent();
    let mut response = agent
        .post(endpoint)
        .header("Accept", "application/json")
        .header("Authorization", &format!("Bearer {api_key}"))
        .send_json(json!({
            "Query": query,
            "DocCount": max_results,
            "MaxSnippetLength": MAX_SNIPPET_CHARS,
            "MaxImageCountPerDoc": 0,
        }))
        .map_err(|error| classify_request_error("Doubao Search", error))?;
    let bytes = read_response_bytes(&mut response)?;
    let payload: Value = serde_json::from_slice(&bytes)
        .map_err(|error| SearchError::Transient(format!("invalid JSON response: {error}")))?;
    if let Some((code, message)) = doubao_business_error(&payload) {
        let detail = format!("Doubao Search rejected the request ({code}): {message}");
        return if matches!(
            code.as_str(),
            "rate_limit_exceeded" | "internal_error" | "service_unavailable"
        ) {
            Err(SearchError::Transient(detail))
        } else {
            Err(SearchError::Permanent(detail))
        };
    }
    let documents = payload
        .pointer("/Result/Documents")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SearchError::Transient("Doubao Search response has no result array".into())
        })?;
    Ok(documents
        .iter()
        .filter_map(|document| {
            let url = document.get("Url")?.as_str()?.trim().to_owned();
            (!url.is_empty()).then(|| {
                json!({
                    "title": truncate_text(
                        document.get("Title").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        MAX_TITLE_CHARS,
                    ),
                    "url": truncate_text(url, MAX_URL_CHARS),
                    "snippet": truncate_text(
                        doubao_snippet(document.get("Snippet")),
                        MAX_SNIPPET_CHARS,
                    ),
                })
            })
        })
        .take(max_results)
        .collect())
}

fn doubao_business_error(payload: &Value) -> Option<(String, String)> {
    if let Some(error) = payload.pointer("/ResponseMetadata/Error")
        && !error.is_null()
    {
        return Some((
            json_value_text(error.get("Code")).unwrap_or_else(|| "unknown_error".into()),
            json_value_text(error.get("Message")).unwrap_or_else(|| "request failed".into()),
        ));
    }
    let result = payload.get("Result")?.as_object()?;
    let code = json_value_text(result.get("ErrorCode"))?;
    if code.is_empty() || code == "0" || code.eq_ignore_ascii_case("success") {
        return None;
    }
    let message = json_value_text(result.get("ErrorMsg"))
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "request failed".into());
    Some((code, message))
}

fn json_value_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn doubao_snippet(value: Option<&Value>) -> String {
    let parts = value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| {
            part.get("Type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("text"))
        })
        .filter_map(|part| part.get("Text").and_then(Value::as_str));
    single_line(parts.collect::<Vec<_>>().join(" "))
}

fn read_response_bytes(
    response: &mut ureq::http::Response<ureq::Body>,
) -> Result<Vec<u8>, SearchError> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| SearchError::Transient(format!("failed to read response: {error}")))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(SearchError::Transient("response exceeded 512 KiB".into()));
    }
    Ok(bytes)
}

fn classify_request_error(provider: &str, error: ureq::Error) -> SearchError {
    match error {
        ureq::Error::StatusCode(status) if (400..500).contains(&status) && status != 429 => {
            SearchError::Permanent(format!("{provider} rejected the request (HTTP {status})"))
        }
        ureq::Error::StatusCode(status) => {
            SearchError::Transient(format!("{provider} returned HTTP {status}"))
        }
        error => SearchError::Transient(format!("{provider} request failed: {error}")),
    }
}

fn single_line(value: String) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut result: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    result.push_str("...");
    result
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    fn searxng(name: &str, base_url: String) -> SearchEngineProfile {
        SearchEngineProfile {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            kind: SearchEngineKind::Searxng,
            base_url,
            enabled: true,
            position: 0,
            has_api_key: false,
        }
    }

    fn doubao_global(name: &str, endpoint: String) -> SearchEngineProfile {
        SearchEngineProfile {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            kind: SearchEngineKind::DoubaoGlobal,
            base_url: endpoint,
            enabled: true,
            position: 0,
            has_api_key: true,
        }
    }

    fn mock_server(status: &str, body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let body = body.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first_line = String::new();
            reader.read_line(&mut first_line).unwrap();
            request.push_str(&first_line);
            let mut content_length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    request.push_str(&header);
                    break;
                }
                if let Some(value) = header
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim())
                {
                    content_length = value.parse::<usize>().unwrap();
                }
                request.push_str(&header);
            }
            if content_length > 0 {
                let mut payload = vec![0; content_length];
                reader.read_exact(&mut payload).unwrap();
                request.push_str(&String::from_utf8(payload).unwrap());
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn truncates_without_splitting_unicode() {
        assert_eq!(truncate_text("abcdef".into(), 4), "abc...");
        assert_eq!(truncate_text("中文内容".into(), 3), "中文...");
    }

    #[test]
    fn requires_an_enabled_engine() {
        let error = execute(
            &[],
            &SearchEngineApiKeys::default(),
            WebSearchArguments {
                query: "test".into(),
                max_results: None,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, "search_unconfigured");
    }

    #[test]
    fn sends_a_searxng_request_and_maps_results() {
        let (base_url, server) = mock_server(
            "200 OK",
            r#"{"results":[{"title":"Pi-Whim","url":"https://example.test/pi-whim","content":"A result"},{"title":"Ignored","content":"Missing URL"}]}"#,
        );
        let result = execute(
            &[searxng("Primary", base_url)],
            &SearchEngineApiKeys::default(),
            WebSearchArguments {
                query: "rust & Pi".into(),
                max_results: Some(2),
            },
            None,
        )
        .unwrap();

        let request = server.join().unwrap();
        assert!(request.starts_with("GET /search?"));
        assert!(request.contains("q=rust%20%26%20Pi"));
        assert!(request.contains("format=json"));
        assert!(request.contains("categories=general"));
        assert_eq!(result["provider"]["name"], "Primary");
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["title"], "Pi-Whim");
        assert_eq!(result["results"][0]["url"], "https://example.test/pi-whim");
        assert_eq!(result["results"][0]["snippet"], "A result");
    }

    #[test]
    fn falls_back_after_a_transient_engine_failure() {
        let (unavailable_url, unavailable) = mock_server("503 Service Unavailable", "{}");
        let (fallback_url, fallback) = mock_server(
            "200 OK",
            r#"{"results":[{"title":"Fallback","url":"https://fallback.test","content":"Used"}]}"#,
        );
        let result = execute(
            &[
                searxng("Unavailable", unavailable_url),
                searxng("Fallback", fallback_url),
            ],
            &SearchEngineApiKeys::default(),
            WebSearchArguments {
                query: "test".into(),
                max_results: None,
            },
            None,
        )
        .unwrap();

        unavailable.join().unwrap();
        fallback.join().unwrap();
        assert_eq!(result["provider"]["name"], "Fallback");
        assert_eq!(result["results"][0]["url"], "https://fallback.test");
    }

    #[test]
    fn posts_doubao_global_request_and_maps_documents() {
        let (endpoint, server) = mock_server(
            "200 OK",
            r#"{"Result":{"Documents":[{"Url":"https://example.test/result","Title":"Result title","Snippet":[{"Type":"text","Text":"first line\n  second line"},{"Type":"image","Text":"ignored"}]},{"Url":"","Title":"Ignored","Snippet":[]}]}}"#,
        );
        let profile = doubao_global("Doubao", endpoint);
        let mut api_keys = SearchEngineApiKeys::default();
        api_keys.insert(profile.id, "test-secret".into());

        let result = execute(
            &[profile],
            &api_keys,
            WebSearchArguments {
                query: "Rust search".into(),
                max_results: Some(2),
            },
            None,
        )
        .unwrap();

        let request = server.join().unwrap();
        assert!(request.starts_with("POST / HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-secret")
        );
        let payload = request.split("\r\n\r\n").nth(1).unwrap();
        let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(payload["Query"], "Rust search");
        assert_eq!(payload["DocCount"], 2);
        assert_eq!(payload["MaxSnippetLength"], MAX_SNIPPET_CHARS);
        assert_eq!(payload["MaxImageCountPerDoc"], 0);
        assert_eq!(result["provider"]["kind"], "doubao_global");
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["snippet"], "first line second line");
    }

    #[test]
    fn doubao_http_success_with_business_error_is_rejected() {
        let (endpoint, server) = mock_server(
            "200 OK",
            r#"{"ResponseMetadata":{"RequestId":"request-id","Error":{"CodeN":700901,"Code":"invalid_api_key","Message":"invalid api key"}},"Result":null}"#,
        );
        let profile = doubao_global("Doubao", endpoint);

        let error = test_engine(&profile, Some("invalid")).unwrap_err();

        server.join().unwrap();
        assert!(error.contains("invalid_api_key"));
        assert!(error.contains("invalid api key"));
    }

    #[test]
    fn doubao_requires_a_key_without_disclosing_it_in_debug_output() {
        let profile = doubao_global(
            "Doubao",
            SearchEngineKind::DoubaoGlobal.default_base_url().into(),
        );
        let error = test_engine(&profile, None).unwrap_err();
        assert!(error.contains("no API key in Keychain"));

        let mut api_keys = SearchEngineApiKeys::default();
        api_keys.insert(profile.id, "do-not-print-this".into());
        let debug = format!("{api_keys:?}");
        assert!(!debug.contains("do-not-print-this"));
        assert!(debug.contains("entries: 1"));
    }
}
