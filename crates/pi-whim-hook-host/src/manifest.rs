//! Strict manifest and wire-shape validation.

use crate::{HookHostError, HookHostResult};
use serde::de::{self, Deserializer};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// Maximum number of hook definitions in one manifest.
pub const MAX_HOOKS: usize = 64;
/// Maximum UTF-8 byte length of a hook identifier.
pub const MAX_ID_BYTES: usize = 128;
/// Maximum number of command entries, including the executable.
pub const MAX_COMMAND_ITEMS: usize = 16;
/// Maximum UTF-8 byte length of one command entry.
pub const MAX_COMMAND_ARG_BYTES: usize = 4 * 1024;
/// Maximum combined UTF-8 byte length of command entries.
pub const MAX_COMMAND_TOTAL_BYTES: usize = 16 * 1024;
/// Maximum number of authorized fields on one definition.
pub const MAX_FIELDS: usize = 64;
/// Maximum number of matcher keys on one definition.
pub const MAX_MATCHER_KEYS: usize = 16;
/// Maximum bytes emitted by a one-shot or persistent hook stdout line.
pub const MAX_STDOUT_BYTES: usize = 64 * 1024;
/// Maximum hook timeout in milliseconds.
pub const MAX_TIMEOUT_MS: u64 = 30_000;
/// Default hook timeout in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Maximum entries in an observe queue when a delivery policy does not override it.
pub const DEFAULT_OBSERVE_QUEUE_CAPACITY: usize = 64;

/// Registry data classification for a payload field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDataClass {
    /// Any bounded JSON value which contains no prohibited nested field.
    Any,
    /// User-controlled content such as message text or tool arguments.
    UserContent,
    /// A bounded public string.
    PublicString,
    /// A project metadata string.
    ProjectMetadata,
    /// A JSON number.
    Number,
    /// A JSON boolean.
    Boolean,
    /// A JSON array.
    Array,
    /// A JSON object.
    Object,
    /// Permanently prohibited secret data.
    Secret,
    /// Permanently prohibited capability data.
    Capability,
    /// Permanently prohibited environment data.
    Environment,
    /// Permanently prohibited API-key data.
    ApiKey,
    /// Permanently prohibited approval-ticket data.
    ApprovalTicket,
    /// Permanently prohibited endpoint data.
    Endpoint,
}

impl HookDataClass {
    pub(crate) fn is_forbidden(self) -> bool {
        matches!(
            self,
            Self::Secret
                | Self::Capability
                | Self::Environment
                | Self::ApiKey
                | Self::ApprovalTicket
                | Self::Endpoint
        )
    }
}

/// The execution phase a hook participates in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookKind {
    /// A deny-capable control hook.
    #[default]
    Gate,
    /// A hook which can return an authorized payload delta.
    Transform,
    /// A best-effort notification hook.
    Observe,
}

impl Serialize for HookKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match self {
            Self::Gate => "gate",
            Self::Transform => "transform",
            Self::Observe => "observe",
        })
    }
}

impl<'de> Deserialize<'de> for HookKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "gate" => Ok(Self::Gate),
            "transform" => Ok(Self::Transform),
            "observe" => Ok(Self::Observe),
            _ => Err(de::Error::custom(format!("unknown hook kind {value}"))),
        }
    }
}

/// How a persistent hook receives observe events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeliveryMode {
    /// A request must be answered, and one control request is allowed in flight.
    #[default]
    RequestResponse,
    /// Keep only the latest pending state event.
    StateLatest,
    /// Queue telemetry up to the configured bounded capacity.
    Telemetry,
}

impl Serialize for DeliveryMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match self {
            Self::RequestResponse => "request_response",
            Self::StateLatest => "state_latest",
            Self::Telemetry => "telemetry",
        })
    }
}

impl<'de> Deserialize<'de> for DeliveryMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "request_response" => Ok(Self::RequestResponse),
            "state_latest" | "latest" => Ok(Self::StateLatest),
            "telemetry" => Ok(Self::Telemetry),
            _ => Err(de::Error::custom(format!("unknown delivery mode {value}"))),
        }
    }
}

/// Bounded delivery configuration for one v2 definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDelivery {
    /// Delivery queueing mode.
    #[serde(default)]
    pub mode: DeliveryMode,
    /// Maximum pending observe requests.
    #[serde(default = "default_delivery_capacity")]
    pub capacity: usize,
}

impl Default for HookDelivery {
    fn default() -> Self {
        Self {
            mode: DeliveryMode::RequestResponse,
            capacity: 1,
        }
    }
}

fn default_delivery_capacity() -> usize {
    1
}

impl HookDelivery {
    pub(crate) fn validate(&self, kind: HookKind, hook_id: &str) -> HookHostResult<()> {
        if !(1..=DEFAULT_OBSERVE_QUEUE_CAPACITY).contains(&self.capacity) {
            return Err(HookHostError::InvalidManifest(format!(
                "hook {hook_id} delivery capacity must be between 1 and {DEFAULT_OBSERVE_QUEUE_CAPACITY}"
            )));
        }
        if !matches!(kind, HookKind::Observe) && self.mode != DeliveryMode::RequestResponse {
            return Err(HookHostError::InvalidManifest(format!(
                "hook {hook_id} non-observe definitions must use request_response delivery"
            )));
        }
        Ok(())
    }
}

/// Restart budget and bounded backoff for a persistent v2 hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRestartPolicy {
    /// Maximum automatic restarts after the initial process.
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    /// Initial restart delay in milliseconds.
    #[serde(default = "default_initial_backoff")]
    pub initial_backoff_ms: u64,
    /// Maximum restart delay in milliseconds.
    #[serde(default = "default_max_backoff")]
    pub max_backoff_ms: u64,
}

impl Default for HookRestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: default_max_restarts(),
            initial_backoff_ms: default_initial_backoff(),
            max_backoff_ms: default_max_backoff(),
        }
    }
}

fn default_max_restarts() -> u32 {
    3
}

fn default_initial_backoff() -> u64 {
    250
}

fn default_max_backoff() -> u64 {
    5_000
}

impl HookRestartPolicy {
    pub(crate) fn validate(&self, hook_id: &str) -> HookHostResult<()> {
        if self.max_restarts > 3 {
            return Err(HookHostError::InvalidManifest(format!(
                "hook {hook_id} restart budget cannot exceed 3"
            )));
        }
        if self.initial_backoff_ms < 250
            || self.initial_backoff_ms > 5_000
            || self.max_backoff_ms == 0
            || self.max_backoff_ms > 5_000
            || self.initial_backoff_ms > self.max_backoff_ms
        {
            return Err(HookHostError::InvalidManifest(format!(
                "hook {hook_id} restart backoff must be within 250..=5000 ms"
            )));
        }
        Ok(())
    }

    pub(crate) fn delay_for(&self, restart_number: u32) -> std::time::Duration {
        let multiplier = 2_u64.saturating_pow(restart_number.min(10));
        std::time::Duration::from_millis(
            self.initial_backoff_ms
                .saturating_mul(multiplier)
                .min(self.max_backoff_ms),
        )
    }
}

/// A matcher with the legacy `tools` and `agent_levels` fields plus a bounded,
/// registry-checked set of simple event-specific keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookMatcher {
    /// Match a payload's top-level `tool` string.
    pub tools: Vec<String>,
    /// Match a payload's numeric `agent_level`.
    pub agent_levels: Vec<u8>,
    /// Additional registry-defined matcher values.
    pub extra: BTreeMap<String, Value>,
}

impl HookMatcher {
    /// Creates the v1-compatible matcher.
    pub fn new(tools: Vec<String>, agent_levels: Vec<u8>) -> Self {
        Self {
            tools,
            agent_levels,
            extra: BTreeMap::new(),
        }
    }

    /// Adds one additional matcher value.
    pub fn with_extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    /// Returns all matcher keys, including legacy keys that are present.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        let mut keys = Vec::new();
        if !self.tools.is_empty() {
            keys.push("tools");
        }
        if !self.agent_levels.is_empty() {
            keys.push("agent_levels");
        }
        keys.extend(self.extra.keys().map(String::as_str));
        keys.into_iter()
    }

    /// Returns whether this matcher selects the supplied payload.
    pub fn matches(&self, payload: &Value) -> bool {
        if !self.tools.is_empty()
            && !payload
                .get("tool")
                .and_then(Value::as_str)
                .is_some_and(|tool| self.tools.iter().any(|candidate| candidate == tool))
        {
            return false;
        }
        if !self.agent_levels.is_empty()
            && !payload
                .get("agent_level")
                .and_then(Value::as_u64)
                .is_some_and(|level| self.agent_levels.contains(&(level as u8)))
        {
            return false;
        }
        self.extra.iter().all(|(key, expected)| {
            let Some(actual) = payload.get(key) else {
                return false;
            };
            match expected {
                Value::Array(values) => values.iter().any(|candidate| candidate == actual),
                _ => actual == expected,
            }
        })
    }
}

impl Serialize for HookMatcher {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        if !self.tools.is_empty() {
            map.serialize_entry("tools", &self.tools)?;
        }
        if !self.agent_levels.is_empty() {
            map.serialize_entry("agent_levels", &self.agent_levels)?;
        }
        for (key, value) in &self.extra {
            if key != "tools" && key != "agent_levels" {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for HookMatcher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let map = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let mut matcher = Self::default();
        for (key, value) in map {
            match key.as_str() {
                "tools" => {
                    matcher.tools = serde_json::from_value(value).map_err(|error| {
                        de::Error::custom(format!("invalid tools matcher: {error}"))
                    })?;
                }
                "agent_levels" => {
                    matcher.agent_levels = serde_json::from_value(value).map_err(|error| {
                        de::Error::custom(format!("invalid agent_levels matcher: {error}"))
                    })?;
                }
                key => {
                    matcher.extra.insert(key.to_owned(), value);
                }
            }
        }
        Ok(matcher)
    }
}

/// A single hook definition after v1 adaptation or v2 parsing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HookDefinition {
    /// Stable hook identifier.
    pub id: String,
    /// Legacy snake-case or v2 namespaced event string.
    pub event: String,
    /// Execution phase.
    #[serde(default)]
    pub kind: HookKind,
    /// Absolute executable followed by optional arguments.
    pub command: Vec<String>,
    /// Per-invocation deadline. Omitted means five seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Authorized top-level payload fields.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Event-specific matcher.
    #[serde(default)]
    pub matcher: HookMatcher,
    /// Persistent delivery policy.
    #[serde(default)]
    pub delivery: HookDelivery,
    /// Persistent restart policy.
    #[serde(default)]
    pub restart: HookRestartPolicy,
    /// Approved command-entrypoint fingerprint supplied by the caller, never by wire.
    #[serde(skip)]
    pub entrypoint_fingerprint: Option<String>,
    #[serde(skip)]
    pub(crate) fields_explicit: bool,
}

impl HookDefinition {
    /// Returns the effective invocation timeout.
    pub fn effective_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        )
    }

    /// Returns a copy associated with an approved entrypoint fingerprint.
    pub fn with_entrypoint_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.entrypoint_fingerprint = Some(fingerprint.into());
        self
    }
}

/// A strict manifest supporting both the existing v1 wire shape and v2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookManifest {
    /// Manifest protocol version, either 1 or 2.
    pub version: u32,
    /// Definitions in the order in which they must execute.
    pub hooks: Vec<HookDefinition>,
    /// Caller-supplied revision used for scope identity; not a wire field.
    pub revision: String,
}

impl Default for HookManifest {
    fn default() -> Self {
        Self {
            version: 1,
            hooks: Vec::new(),
            revision: String::new(),
        }
    }
}

impl HookManifest {
    /// Creates a manifest without a caller-supplied revision.
    pub fn new(version: u32, hooks: Vec<HookDefinition>) -> Self {
        Self {
            version,
            hooks,
            revision: String::new(),
        }
    }

    /// Associates a revision with the manifest for scope management.
    pub fn with_revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = revision.into();
        self
    }

    /// Parses and validates a manifest against the built-in registry.
    pub fn parse_json(input: &str) -> HookHostResult<Self> {
        let manifest = serde_json::from_str::<Self>(input)
            .map_err(|error| HookHostError::InvalidManifest(error.to_string()))?;
        manifest.validate(&crate::EventRegistry::default())?;
        Ok(manifest)
    }

    /// Validates the full manifest against an event registry.
    pub fn validate(&self, registry: &crate::EventRegistry) -> HookHostResult<()> {
        if !matches!(self.version, 1 | 2) {
            return Err(HookHostError::InvalidManifest(format!(
                "unsupported hook manifest version {}",
                self.version
            )));
        }
        if self.hooks.len() > MAX_HOOKS {
            return Err(HookHostError::InvalidManifest(format!(
                "manifest cannot contain more than {MAX_HOOKS} hooks"
            )));
        }
        let mut ids = std::collections::HashSet::new();
        for hook in &self.hooks {
            validate_definition_basics(hook)?;
            if !ids.insert(hook.id.as_str()) {
                return Err(HookHostError::InvalidManifest(format!(
                    "duplicate hook id {}",
                    hook.id
                )));
            }
            registry.validate_definition(self.version, hook)?;
        }
        Ok(())
    }

    /// Returns a copy with entrypoint fingerprints applied by hook id.
    pub fn with_entrypoint_fingerprints(
        mut self,
        fingerprints: &BTreeMap<String, String>,
    ) -> HookHostResult<Self> {
        let hook_ids = self
            .hooks
            .iter()
            .map(|hook| hook.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        for (hook_id, fingerprint) in fingerprints {
            if !hook_ids.contains(hook_id.as_str()) {
                return Err(HookHostError::InvalidManifest(format!(
                    "entrypoint fingerprint supplied for unknown hook {hook_id}"
                )));
            }
            if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(HookHostError::InvalidManifest(format!(
                    "entrypoint fingerprint for hook {hook_id} must be 64 hexadecimal bytes"
                )));
            }
        }
        for hook in &mut self.hooks {
            if let Some(fingerprint) = fingerprints.get(&hook.id) {
                hook.entrypoint_fingerprint = Some(fingerprint.clone());
            }
        }
        Ok(self)
    }
}

impl Serialize for HookManifest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("version", &self.version)?;
        if self.version == 1 {
            let hooks = self
                .hooks
                .iter()
                .map(V1HookDefinitionWire::from_definition)
                .collect::<Vec<_>>();
            map.serialize_entry("hooks", &hooks)?;
        } else {
            let hooks = self
                .hooks
                .iter()
                .map(V2HookDefinitionWire::from_definition)
                .collect::<Vec<_>>();
            map.serialize_entry("hooks", &hooks)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for HookManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| de::Error::custom("hook manifest must be an object"))?;
        reject_unknown_keys(object, &["version", "hooks"]).map_err(de::Error::custom)?;
        let version = match object.get("version") {
            None => 1,
            Some(value) => value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| de::Error::custom("manifest version must be an unsigned integer"))?,
        };
        let hooks_value = object
            .get("hooks")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        if !hooks_value.is_array() {
            return Err(de::Error::custom("manifest hooks must be an array"));
        }
        match version {
            1 => {
                let wire = ManifestV1Wire {
                    version,
                    hooks: serde_json::from_value(hooks_value)
                        .map_err(|error| de::Error::custom(error.to_string()))?,
                };
                Ok(Self {
                    version: wire.version,
                    hooks: wire
                        .hooks
                        .into_iter()
                        .map(HookDefinition::from_v1)
                        .collect(),
                    revision: String::new(),
                })
            }
            2 => {
                let hooks = serde_json::from_value::<Vec<V2HookDefinitionWire>>(hooks_value)
                    .map_err(|error| de::Error::custom(error.to_string()))?;
                Ok(Self {
                    version,
                    hooks: hooks.into_iter().map(HookDefinition::from_v2).collect(),
                    revision: String::new(),
                })
            }
            _ => Err(de::Error::custom(format!(
                "unsupported hook manifest version {version}"
            ))),
        }
    }
}

fn validate_definition_basics(hook: &HookDefinition) -> HookHostResult<()> {
    if hook.id.is_empty()
        || hook.id.len() > MAX_ID_BYTES
        || !hook
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(HookHostError::InvalidManifest(format!(
            "hook id {} is not a valid ASCII identifier of at most {MAX_ID_BYTES} bytes",
            hook.id
        )));
    }
    if hook.command.is_empty() || hook.command.len() > MAX_COMMAND_ITEMS {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} command must contain 1..={MAX_COMMAND_ITEMS} entries",
            hook.id
        )));
    }
    if !Path::new(&hook.command[0]).is_absolute() {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} command executable must be absolute",
            hook.id
        )));
    }
    if hook
        .command
        .iter()
        .any(|entry| entry.len() > MAX_COMMAND_ARG_BYTES)
        || hook.command.iter().map(String::len).sum::<usize>() > MAX_COMMAND_TOTAL_BYTES
    {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} command exceeds byte limits",
            hook.id
        )));
    }
    if hook
        .timeout_ms
        .is_some_and(|timeout| !(1..=MAX_TIMEOUT_MS).contains(&timeout))
    {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} timeout must be between 1 and {MAX_TIMEOUT_MS} ms",
            hook.id
        )));
    }
    if hook.fields.len() > MAX_FIELDS {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} has too many authorized fields",
            hook.id
        )));
    }
    let mut fields = std::collections::HashSet::new();
    if hook.fields.iter().any(|field| {
        field.is_empty()
            || field.len() > MAX_ID_BYTES
            || is_forbidden_field_name(field)
            || !fields.insert(field.as_str())
    }) {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} has an invalid, duplicate, or permanently prohibited authorized field",
            hook.id
        )));
    }
    let matcher_count = hook.matcher.keys().count();
    if matcher_count > MAX_MATCHER_KEYS {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} has too many matcher keys",
            hook.id
        )));
    }
    if hook
        .matcher
        .extra
        .values()
        .any(|value| serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > 4 * 1024))
    {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {} matcher value exceeds byte limits",
            hook.id
        )));
    }
    hook.delivery.validate(hook.kind, &hook.id)?;
    hook.restart.validate(&hook.id)?;
    Ok(())
}

fn reject_unknown_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("unknown field {key}"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestV1Wire {
    #[serde(default = "default_manifest_version")]
    version: u32,
    #[serde(default)]
    hooks: Vec<V1HookDefinitionWire>,
}

fn default_manifest_version() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1HookDefinitionWire {
    id: String,
    event: String,
    #[serde(default)]
    kind: HookKind,
    command: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    matcher: V1HookMatcherWire,
}

impl V1HookDefinitionWire {
    fn from_definition(definition: &HookDefinition) -> Self {
        Self {
            id: definition.id.clone(),
            event: definition.event.clone(),
            kind: definition.kind,
            command: definition.command.clone(),
            timeout_ms: definition.timeout_ms,
            matcher: V1HookMatcherWire::from_matcher(&definition.matcher),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1HookMatcherWire {
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    agent_levels: Vec<u8>,
}

impl V1HookMatcherWire {
    fn from_matcher(matcher: &HookMatcher) -> Self {
        Self {
            tools: matcher.tools.clone(),
            agent_levels: matcher.agent_levels.clone(),
        }
    }

    fn into_matcher(self) -> HookMatcher {
        HookMatcher::new(self.tools, self.agent_levels)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V2HookDefinitionWire {
    id: String,
    event: String,
    #[serde(default)]
    kind: HookKind,
    command: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    matcher: HookMatcher,
    #[serde(default)]
    delivery: HookDelivery,
    #[serde(default)]
    restart: HookRestartPolicy,
}

impl V2HookDefinitionWire {
    fn from_definition(definition: &HookDefinition) -> Self {
        Self {
            id: definition.id.clone(),
            event: definition.event.clone(),
            kind: definition.kind,
            command: definition.command.clone(),
            timeout_ms: definition.timeout_ms,
            fields: definition.fields.clone(),
            matcher: definition.matcher.clone(),
            delivery: definition.delivery.clone(),
            restart: definition.restart.clone(),
        }
    }
}

impl HookDefinition {
    fn from_v1(wire: V1HookDefinitionWire) -> Self {
        Self {
            id: wire.id,
            event: wire.event,
            kind: wire.kind,
            command: wire.command,
            timeout_ms: wire.timeout_ms,
            fields: Vec::new(),
            matcher: wire.matcher.into_matcher(),
            delivery: HookDelivery::default(),
            restart: HookRestartPolicy::default(),
            entrypoint_fingerprint: None,
            fields_explicit: false,
        }
    }

    fn from_v2(wire: V2HookDefinitionWire) -> Self {
        Self {
            id: wire.id,
            event: wire.event,
            kind: wire.kind,
            command: wire.command,
            timeout_ms: wire.timeout_ms,
            fields: wire.fields,
            matcher: wire.matcher,
            delivery: wire.delivery,
            restart: wire.restart,
            entrypoint_fingerprint: None,
            fields_explicit: true,
        }
    }
}

/// Returns whether a field name is permanently prohibited from export.
pub(crate) fn is_forbidden_field_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace(['-', '.'], "_");
    let parts = normalized.split('_').collect::<Vec<_>>();
    let exact = matches!(
        normalized.as_str(),
        "secret"
            | "capability"
            | "environment"
            | "api_key"
            | "apikey"
            | "approval_ticket"
            | "endpoint"
            | "credential"
            | "authorization"
            | "access_token"
            | "token"
    );
    let prefixed = parts.iter().any(|part| {
        matches!(
            *part,
            "secret"
                | "capability"
                | "environment"
                | "env"
                | "apikey"
                | "approval"
                | "endpoint"
                | "credential"
                | "authorization"
                | "token"
        )
    });
    exact || prefixed
}
