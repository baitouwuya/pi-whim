use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use pi_whim_core::{SearchEngineKind, SearchEngineProfile};
use serde::Deserialize;
use serde_json::json;

use crate::{HostError, HostResult};

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
            "No enabled web search engine is configured. Add a SearXNG engine in Settings > Web Search.",
        ));
    }

    let mut failures = Vec::new();
    for engine in enabled {
        if cancelled.is_some_and(|value| value.load(Ordering::Relaxed)) {
            return Err(HostError::new("cancelled", "web search was cancelled"));
        }
        match search_engine(engine, query, max_results) {
            Ok(results) => {
                return Ok(json!({
                    "query": query,
                    "provider": { "name": engine.name, "kind": "searxng" },
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
    query: &str,
    max_results: usize,
) -> Result<Vec<serde_json::Value>, SearchError> {
    match engine.kind {
        SearchEngineKind::Searxng => search_searxng(engine, query, max_results),
    }
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
        .map_err(classify_request_error)?;

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
                    "snippet": truncate_text(result.content.unwrap_or_default(), MAX_SNIPPET_CHARS),
                })
            })
        })
        .take(max_results)
        .collect())
}

fn classify_request_error(error: ureq::Error) -> SearchError {
    match error {
        ureq::Error::StatusCode(status) if (400..500).contains(&status) && status != 429 => {
            SearchError::Permanent(format!("SearXNG rejected the request (HTTP {status})"))
        }
        ureq::Error::StatusCode(status) => {
            SearchError::Transient(format!("SearXNG returned HTTP {status}"))
        }
        error => SearchError::Transient(format!("SearXNG request failed: {error}")),
    }
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
            reader.read_line(&mut request).unwrap();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
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
}
