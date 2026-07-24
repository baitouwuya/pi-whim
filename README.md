# Pi-Whim

Pi-Whim is a native Rust desktop workbench for the [Pi coding agent](https://pi.dev).
It owns project navigation, session history, credentials, and presentation while Pi
continues to own the agent loop and JSONL session format.

## Development

1. Install Rust stable, Node.js 22.19 or newer, and Bun.
2. Initialise the source dependency: `git submodule update --init --recursive`.
3. Build Pi once: `cargo run -p xtask -- pi-build`.
4. Start the application: `cargo run -p pi-whim-app`.

`PI_WHIM_PI_BIN` can point to a standalone Pi binary during development. Otherwise
the application looks for the Pi build under `vendor/pi-mono`.

## Architecture

- `pi-whim-core`: application domain and reducer.
- `pi-whim-agent-team`: authenticated team topology, routing, quotas, and Pi process supervision.
- `pi-whim-persistence`: SQLite project/session index and Keychain secrets.
- `pi-whim-pi-rpc`: strict LF JSONL transport for Pi RPC mode.
- `pi-whim-runtime`: UI-facing `AgentRuntime` abstraction and Pi adapter.
- `pi-whim-tool-protocol`: versioned JSONL protocol used by the local agent tool host.
- `pi-whim-ui`: egui workbench and Fluent resources.
- `pi-whim-app`: native executable composition root.

Pi is kept as a Git submodule in `vendor/pi-mono`. The initial checkout uses upstream
unchanged; a project-owned fork can later replace its `origin` while `upstream` stays
pointed to the official repository.

See [Agent teams](docs/agent-teams.md) for hierarchy, communication, model selection,
and concurrency rules.

## Providers

Open Settings > Providers to add a provider with a name, Base URL, API key, request
protocol, and one or more models. The presets only fill these fields; they do not add
credentials or make network requests.

- `OpenAI Chat Completions` is the broadest OpenAI-compatible choice and discovers
  models from `GET <base-url>/models` using `Authorization: Bearer <key>`.
- `OpenAI Responses` uses the same discovery endpoint but selects Pi's
  `openai-responses` request shape.
- `Anthropic Messages` discovers through `GET <base-url>/v1/models`, with
  `x-api-key` and `anthropic-version: 2023-06-01`.
- `Google Generative AI` discovers through `GET <base-url>/models`, with
  `x-goog-api-key`; `models/` prefixes are removed from returned IDs.

Discovery is optional: proxies do not always expose a catalogue, so a model ID can
always be added manually. Provider metadata and model IDs are stored in SQLite; the
API key is stored only in macOS Keychain. Before Pi starts, Pi-Whim writes an
application-owned `models.json` containing environment-variable references, then
injects each key only into that Pi process.

## Web Search

Open Settings > Web Search to add one or more SearXNG base URLs. Engines are tried in
the displayed order: temporary errors (including timeouts, HTTP 429, and 5xx responses)
fall through to the next enabled engine, while valid empty results are returned as-is.
Use the test action to verify an endpoint before saving; the `web_search` agent tool
returns only result titles, URLs, and snippets.

## Network Fetch

The agent `fetch` tool makes a bounded one-shot HTTP(S), TCP, UDP, or WebSocket request.
It supports text and Base64 payloads, limits request bodies to 256 KiB and responses to 1 MiB,
and caps a call at 30 seconds. WebSocket calls send at most one message and read one reply, so
they do not retain sockets or state between agent tool calls. `fetch` is available to controlled
and full agents; read-only agents retain access to `web_search` only.
