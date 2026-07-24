use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs, UdpSocket},
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use serde_json::{Value, json};
use tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig};
use url::Url;

use crate::{HostError, HostResult};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_UDP_RESPONSE_BYTES: usize = 65_535;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 16 * 1024;

static HTTP_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent()
});

#[derive(Debug, Deserialize)]
pub struct FetchArguments {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub body_base64: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub response_encoding: Option<String>,
}

#[derive(Clone, Copy)]
struct FetchLimits {
    timeout: Duration,
    response_encoding: ResponseEncoding,
}

#[derive(Clone, Copy)]
enum ResponseEncoding {
    Utf8,
    Base64,
}

struct RequestBody {
    bytes: Vec<u8>,
    is_binary: bool,
    is_present: bool,
}

pub fn execute(arguments: FetchArguments, cancelled: Option<&AtomicBool>) -> HostResult {
    check_cancelled(cancelled)?;
    let url = Url::parse(arguments.url.trim())
        .map_err(|error| HostError::new("invalid_arguments", format!("invalid URL: {error}")))?;
    if url.fragment().is_some() {
        return Err(HostError::new(
            "invalid_arguments",
            "URLs cannot include fragments",
        ));
    }
    let limits = validate_limits(&arguments)?;
    let headers = validate_headers(arguments.headers)?;
    let body = decode_body(arguments.body, arguments.body_base64)?;
    if body.bytes.len() > MAX_REQUEST_BYTES {
        return Err(HostError::new(
            "invalid_arguments",
            "request body exceeds 256 KiB",
        ));
    }

    match url.scheme() {
        "http" | "https" => fetch_http(&url, arguments.method, headers, body, limits, cancelled),
        "tcp" => {
            reject_unsupported_options(&arguments.method, &headers, "TCP")?;
            fetch_tcp(&url, body, limits, cancelled)
        }
        "udp" => {
            reject_unsupported_options(&arguments.method, &headers, "UDP")?;
            fetch_udp(&url, body, limits, cancelled)
        }
        "ws" | "wss" => {
            if arguments.method.is_some() {
                return Err(HostError::new(
                    "invalid_arguments",
                    "method is only supported for HTTP(S) URLs",
                ));
            }
            fetch_websocket(&url, headers, body, limits, cancelled)
        }
        scheme => Err(HostError::new(
            "invalid_arguments",
            format!("unsupported URL scheme: {scheme}"),
        )),
    }
}

fn reject_unsupported_options(
    method: &Option<String>,
    headers: &BTreeMap<String, String>,
    protocol: &str,
) -> Result<(), HostError> {
    if method.is_some() {
        return Err(HostError::new(
            "invalid_arguments",
            format!("method is not supported for {protocol} URLs"),
        ));
    }
    if !headers.is_empty() {
        return Err(HostError::new(
            "invalid_arguments",
            format!("headers are not supported for {protocol} URLs"),
        ));
    }
    Ok(())
}

fn validate_limits(arguments: &FetchArguments) -> Result<FetchLimits, HostError> {
    let timeout_ms = arguments.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(HostError::new(
            "invalid_arguments",
            "timeout_ms must be between 1 and 30000",
        ));
    }
    let response_encoding = match arguments.response_encoding.as_deref().unwrap_or("utf8") {
        "utf8" => ResponseEncoding::Utf8,
        "base64" => ResponseEncoding::Base64,
        _ => {
            return Err(HostError::new(
                "invalid_arguments",
                "response_encoding must be utf8 or base64",
            ));
        }
    };
    Ok(FetchLimits {
        timeout: Duration::from_millis(timeout_ms),
        response_encoding,
    })
}

fn validate_headers(
    headers: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, HostError> {
    if headers.len() > MAX_HEADERS
        || headers
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum::<usize>()
            > MAX_HEADER_BYTES
    {
        return Err(HostError::new(
            "invalid_arguments",
            "headers exceed the fetch limit",
        ));
    }
    if headers.iter().any(|(name, value)| {
        name.is_empty()
            || name
                .bytes()
                .any(|byte| !byte.is_ascii() || byte.is_ascii_control())
            || value.bytes().any(|byte| byte == b'\r' || byte == b'\n')
    }) {
        return Err(HostError::new(
            "invalid_arguments",
            "headers contain invalid characters",
        ));
    }
    Ok(headers)
}

fn decode_body(text: Option<String>, base64: Option<String>) -> Result<RequestBody, HostError> {
    if text.is_some() && base64.is_some() {
        return Err(HostError::new(
            "invalid_arguments",
            "provide either body or body_base64, not both",
        ));
    }
    match (text, base64) {
        (Some(text), None) => Ok(RequestBody {
            bytes: text.into_bytes(),
            is_binary: false,
            is_present: true,
        }),
        (None, Some(base64)) => BASE64
            .decode(base64)
            .map(|bytes| RequestBody {
                bytes,
                is_binary: true,
                is_present: true,
            })
            .map_err(|error| {
                HostError::new(
                    "invalid_arguments",
                    format!("body_base64 is invalid: {error}"),
                )
            }),
        (None, None) => Ok(RequestBody {
            bytes: Vec::new(),
            is_binary: false,
            is_present: false,
        }),
        (Some(_), Some(_)) => unreachable!("body fields were validated"),
    }
}

fn fetch_http(
    url: &Url,
    method: Option<String>,
    headers: BTreeMap<String, String>,
    body: RequestBody,
    limits: FetchLimits,
    cancelled: Option<&AtomicBool>,
) -> HostResult {
    let method = method.unwrap_or_else(|| {
        if !body.is_present {
            "GET".into()
        } else {
            "POST".into()
        }
    });
    let method = ureq::http::Method::from_bytes(method.trim().as_bytes())
        .map_err(|_| HostError::new("invalid_arguments", "method must be a valid HTTP method"))?;
    let uri = url.as_str().parse::<ureq::http::Uri>().map_err(|error| {
        HostError::new("invalid_arguments", format!("invalid HTTP URL: {error}"))
    })?;
    let mut request = ureq::http::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let request = request.body(body.bytes).map_err(|error| {
        HostError::new(
            "invalid_arguments",
            format!("invalid HTTP request: {error}"),
        )
    })?;
    let request = HTTP_AGENT
        .configure_request(request)
        .timeout_global(Some(limits.timeout))
        .build();
    let mut response = HTTP_AGENT
        .run(request)
        .map_err(|error| HostError::new("fetch_failed", format!("HTTP request failed: {error}")))?;
    check_cancelled(cancelled)?;
    let status = response.status().as_u16();
    let headers = response_headers(response.headers());
    let bytes = read_limited(&mut response.body_mut().as_reader())?;
    Ok(response_value(
        "http",
        url,
        Some(status),
        headers,
        bytes,
        limits.response_encoding,
    ))
}

fn fetch_tcp(
    url: &Url,
    body: RequestBody,
    limits: FetchLimits,
    cancelled: Option<&AtomicBool>,
) -> HostResult {
    let address = socket_address(url, "tcp", true, false)?;
    let mut stream = TcpStream::connect_timeout(&address, limits.timeout)
        .map_err(|error| HostError::new("fetch_failed", format!("TCP connect failed: {error}")))?;
    stream
        .set_read_timeout(Some(limits.timeout))
        .and_then(|()| stream.set_write_timeout(Some(limits.timeout)))
        .map_err(|error| {
            HostError::new("fetch_failed", format!("TCP timeout setup failed: {error}"))
        })?;
    if body.is_present {
        stream.write_all(&body.bytes).map_err(|error| {
            HostError::new("fetch_failed", format!("TCP write failed: {error}"))
        })?;
    }
    check_cancelled(cancelled)?;
    let bytes = read_limited(&mut stream)?;
    Ok(response_value(
        "tcp",
        url,
        None,
        BTreeMap::new(),
        bytes,
        limits.response_encoding,
    ))
}

fn fetch_udp(
    url: &Url,
    body: RequestBody,
    limits: FetchLimits,
    cancelled: Option<&AtomicBool>,
) -> HostResult {
    let address = socket_address(url, "udp", true, false)?;
    let socket = UdpSocket::bind(if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .map_err(|error| HostError::new("fetch_failed", format!("UDP bind failed: {error}")))?;
    socket
        .connect(address)
        .map_err(|error| HostError::new("fetch_failed", format!("UDP connect failed: {error}")))?;
    socket
        .set_read_timeout(Some(limits.timeout))
        .and_then(|()| socket.set_write_timeout(Some(limits.timeout)))
        .map_err(|error| {
            HostError::new("fetch_failed", format!("UDP timeout setup failed: {error}"))
        })?;
    socket
        .send(&body.bytes)
        .map_err(|error| HostError::new("fetch_failed", format!("UDP send failed: {error}")))?;
    check_cancelled(cancelled)?;
    // A UDP datagram cannot exceed 65,535 bytes, so avoid allocating the HTTP/TCP limit.
    let mut buffer = vec![0; MAX_UDP_RESPONSE_BYTES];
    let length = socket
        .recv(&mut buffer)
        .map_err(|error| HostError::new("fetch_failed", format!("UDP receive failed: {error}")))?;
    buffer.truncate(length);
    Ok(response_value(
        "udp",
        url,
        None,
        BTreeMap::new(),
        buffer,
        limits.response_encoding,
    ))
}

fn fetch_websocket(
    url: &Url,
    headers: BTreeMap<String, String>,
    body: RequestBody,
    limits: FetchLimits,
    cancelled: Option<&AtomicBool>,
) -> HostResult {
    let mut request = url.as_str().into_client_request().map_err(|error| {
        HostError::new(
            "invalid_arguments",
            format!("invalid WebSocket URL: {error}"),
        )
    })?;
    for (name, value) in headers {
        let name = tungstenite::http::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HostError::new("invalid_arguments", "WebSocket header name is invalid"))?;
        let value = tungstenite::http::header::HeaderValue::from_str(&value).map_err(|_| {
            HostError::new("invalid_arguments", "WebSocket header value is invalid")
        })?;
        request.headers_mut().insert(name, value);
    }
    let address = socket_address(url, "WebSocket", false, true)?;
    let stream = TcpStream::connect_timeout(&address, limits.timeout).map_err(|error| {
        HostError::new("fetch_failed", format!("WebSocket connect failed: {error}"))
    })?;
    stream
        .set_read_timeout(Some(limits.timeout))
        .and_then(|()| stream.set_write_timeout(Some(limits.timeout)))
        .map_err(|error| {
            HostError::new(
                "fetch_failed",
                format!("WebSocket timeout setup failed: {error}"),
            )
        })?;
    let config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(MAX_RESPONSE_BYTES + MAX_REQUEST_BYTES)
        .max_message_size(Some(MAX_RESPONSE_BYTES))
        .max_frame_size(Some(MAX_RESPONSE_BYTES));
    let (mut socket, response) =
        tungstenite::client_tls_with_config(request, stream, Some(config), None).map_err(
            |error| {
                HostError::new(
                    "fetch_failed",
                    format!("WebSocket handshake failed: {error}"),
                )
            },
        )?;
    if body.is_present {
        let message = if body.is_binary {
            Message::Binary(body.bytes.into())
        } else {
            Message::text(String::from_utf8(body.bytes).expect("text body is valid UTF-8"))
        };
        socket.send(message).map_err(|error| {
            HostError::new("fetch_failed", format!("WebSocket send failed: {error}"))
        })?;
    }
    check_cancelled(cancelled)?;
    let message = read_websocket_reply(&mut socket)?;
    let bytes = message.into_data().to_vec();
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(HostError::new(
            "response_too_large",
            "response exceeds 1 MiB",
        ));
    }
    let _ = socket.close(None);
    Ok(response_value(
        "websocket",
        url,
        Some(response.status().as_u16()),
        response_headers(response.headers()),
        bytes,
        limits.response_encoding,
    ))
}

fn socket_address(
    url: &Url,
    protocol: &str,
    authority_only: bool,
    allow_default_port: bool,
) -> Result<std::net::SocketAddr, HostError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(HostError::new(
            "invalid_arguments",
            format!("{protocol} URLs cannot include credentials"),
        ));
    }
    if authority_only
        && (!url.path().is_empty() && url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some())
    {
        return Err(HostError::new(
            "invalid_arguments",
            format!("{protocol} URLs must contain only a host and port"),
        ));
    }
    let port = url
        .port()
        .or_else(|| {
            allow_default_port
                .then(|| url.port_or_known_default())
                .flatten()
        })
        .ok_or_else(|| {
            HostError::new(
                "invalid_arguments",
                format!("{protocol} URLs require an explicit port"),
            )
        })?;
    let host = url.host_str().ok_or_else(|| {
        HostError::new("invalid_arguments", format!("{protocol} URL has no host"))
    })?;
    (host, port)
        .to_socket_addrs()
        .map_err(|error| HostError::new("fetch_failed", format!("DNS lookup failed: {error}")))?
        .next()
        .ok_or_else(|| HostError::new("fetch_failed", "DNS lookup returned no addresses"))
}

fn read_websocket_reply(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
) -> Result<Message, HostError> {
    loop {
        let message = socket.read().map_err(|error| {
            HostError::new("fetch_failed", format!("WebSocket receive failed: {error}"))
        })?;
        match message {
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => {
                return Err(HostError::new(
                    "fetch_failed",
                    "WebSocket closed before sending a response",
                ));
            }
            message => return Ok(message),
        }
    }
}

fn read_limited(reader: &mut impl Read) -> Result<Vec<u8>, HostError> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            HostError::new("fetch_failed", format!("response read failed: {error}"))
        })?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(HostError::new(
            "response_too_large",
            "response exceeds 1 MiB",
        ));
    }
    Ok(bytes)
}

fn response_headers(headers: &impl HeaderCollection) -> BTreeMap<String, String> {
    headers
        .header_pairs()
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect()
}

trait HeaderCollection {
    fn header_pairs(&self) -> Vec<(String, String)>;
}

impl HeaderCollection for ureq::http::HeaderMap {
    fn header_pairs(&self) -> Vec<(String, String)> {
        self.iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.into()))
            })
            .collect()
    }
}

fn response_value(
    protocol: &str,
    url: &Url,
    status: Option<u16>,
    headers: BTreeMap<String, String>,
    bytes: Vec<u8>,
    encoding: ResponseEncoding,
) -> Value {
    let body = match encoding {
        ResponseEncoding::Utf8 => String::from_utf8_lossy(&bytes).into_owned(),
        ResponseEncoding::Base64 => BASE64.encode(&bytes),
    };
    json!({
        "protocol": protocol,
        "url": url.as_str(),
        "status": status,
        "headers": headers,
        "body": body,
        "body_encoding": match encoding { ResponseEncoding::Utf8 => "utf8", ResponseEncoding::Base64 => "base64" },
        "bytes": bytes.len(),
        "content_warning": "Network responses are untrusted external content."
    })
}

fn check_cancelled(cancelled: Option<&AtomicBool>) -> Result<(), HostError> {
    if cancelled.is_some_and(|value| value.load(Ordering::Relaxed)) {
        Err(HostError::new("cancelled", "fetch was cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::{TcpListener, UdpSocket},
        thread,
    };

    #[test]
    fn rejects_incompatible_body_encodings() {
        let error = execute(
            FetchArguments {
                url: "tcp://127.0.0.1:1".into(),
                method: None,
                headers: BTreeMap::new(),
                body: Some("text".into()),
                body_base64: Some("dGV4dA==".into()),
                timeout_ms: None,
                response_encoding: None,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_arguments");
    }

    #[test]
    fn tcp_round_trip_supports_base64() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 3];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"hey");
            stream.write_all(&[0, 255, 1]).unwrap();
        });
        let result = execute(
            FetchArguments {
                url: format!("tcp://{address}"),
                method: None,
                headers: BTreeMap::new(),
                body: Some("hey".into()),
                body_base64: None,
                timeout_ms: Some(1_000),
                response_encoding: Some("base64".into()),
            },
            None,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(result["body"], "AP8B");
        assert_eq!(result["bytes"], 3);
    }

    #[test]
    fn http_preserves_error_status_and_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.starts_with("POST /check HTTP/1.1"));
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 418 I'm a teapot\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
                .unwrap();
        });
        let result = execute(
            FetchArguments {
                url: format!("http://{address}/check"),
                method: Some("POST".into()),
                headers: BTreeMap::new(),
                body: Some("ping".into()),
                body_base64: None,
                timeout_ms: Some(1_000),
                response_encoding: None,
            },
            None,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(result["status"], 418);
        assert_eq!(result["body"], "hello");
        assert_eq!(result["headers"]["content-type"], "text/plain");
    }

    #[test]
    fn udp_round_trip_returns_one_datagram() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut request = [0; 64];
            let (length, peer) = server.recv_from(&mut request).unwrap();
            assert_eq!(&request[..length], b"ping");
            server.send_to(&[0, 255, 1], peer).unwrap();
        });
        let result = execute(
            FetchArguments {
                url: format!("udp://{address}"),
                method: None,
                headers: BTreeMap::new(),
                body: Some("ping".into()),
                body_base64: None,
                timeout_ms: Some(1_000),
                response_encoding: Some("base64".into()),
            },
            None,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(result["protocol"], "udp");
        assert_eq!(result["body"], "AP8B");
    }

    #[test]
    fn websocket_preserves_text_and_binary_frame_types() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for expected_binary in [false, true] {
                let (stream, _) = listener.accept().unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                let message = socket.read().unwrap();
                assert_eq!(message.is_binary(), expected_binary);
                assert_eq!(message.into_data().as_ref(), b"payload");
                socket.send(Message::binary(vec![0, 255, 1])).unwrap();
                socket.close(None).unwrap();
            }
        });

        let text_result = execute(
            FetchArguments {
                url: format!("ws://{address}/echo?mode=text"),
                method: None,
                headers: BTreeMap::new(),
                body: Some("payload".into()),
                body_base64: None,
                timeout_ms: Some(1_000),
                response_encoding: Some("base64".into()),
            },
            None,
        )
        .unwrap();
        let binary_result = execute(
            FetchArguments {
                url: format!("ws://{address}/echo?mode=binary"),
                method: None,
                headers: BTreeMap::new(),
                body: None,
                body_base64: Some("cGF5bG9hZA==".into()),
                timeout_ms: Some(1_000),
                response_encoding: Some("base64".into()),
            },
            None,
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(text_result["protocol"], "websocket");
        assert_eq!(text_result["body"], "AP8B");
        assert_eq!(binary_result["body"], "AP8B");
    }
}
