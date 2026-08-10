//! Event, kind, field, and matcher authorization registry.

use crate::manifest::{
    HookDataClass, HookDefinition, HookKind, HookManifest, HookMatcher, MAX_FIELDS,
    is_forbidden_field_name,
};
use crate::{HookHostError, HookHostResult, HookPayload};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Authorization and type metadata for one event field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookFieldSpec {
    /// Field name as it appears in the mutable payload.
    pub name: String,
    /// Data classification enforced before serialization.
    pub data_class: HookDataClass,
    /// Whether a transform may change this field.
    pub transformable: bool,
    /// Whether a project-scoped definition may request this field.
    pub project_visible: bool,
}

impl HookFieldSpec {
    /// Creates a field specification, rejecting permanently forbidden names or classes.
    pub fn new(
        name: impl Into<String>,
        data_class: HookDataClass,
        transformable: bool,
        project_visible: bool,
    ) -> HookHostResult<Self> {
        let name = name.into();
        if is_forbidden_field_name(&name) || data_class.is_forbidden() {
            return Err(HookHostError::ForbiddenField { field: name });
        }
        if name.is_empty() || name.len() > 128 {
            return Err(HookHostError::InvalidManifest(
                "hook field name must be 1..=128 bytes".to_owned(),
            ));
        }
        Ok(Self {
            name,
            data_class,
            transformable,
            project_visible,
        })
    }
}

/// The allowed kind matrix and payload metadata for one event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookEventSpec {
    /// Canonical namespaced event name.
    pub event: String,
    /// Legacy event aliases accepted only by v1 manifests.
    pub aliases: Vec<String>,
    /// Allowed kind entries in the event-kind matrix.
    pub kinds: BTreeMap<HookKind, HookKindSpec>,
    /// Fields which may be authorized by a manifest.
    pub fields: BTreeMap<String, HookFieldSpec>,
    /// Whether a project scope may receive this event at all.
    pub project_visible: bool,
    /// Matcher keys accepted for this event.
    pub matcher_keys: BTreeSet<String>,
}

impl HookEventSpec {
    /// Creates an empty event specification.
    pub fn new(event: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            aliases: Vec::new(),
            kinds: BTreeMap::new(),
            fields: BTreeMap::new(),
            project_visible: true,
            matcher_keys: BTreeSet::new(),
        }
    }

    /// Adds a legacy alias.
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Adds an allowed kind to the event matrix.
    pub fn with_kind(mut self, kind: HookKind, spec: HookKindSpec) -> Self {
        self.kinds.insert(kind, spec);
        self
    }

    /// Adds an authorized field.
    pub fn with_field(mut self, field: HookFieldSpec) -> Self {
        self.fields.insert(field.name.clone(), field);
        self
    }

    /// Adds an allowed matcher key.
    pub fn with_matcher_key(mut self, key: impl Into<String>) -> Self {
        self.matcher_keys.insert(key.into());
        self
    }

    /// Marks the event as unavailable to project-scoped definitions.
    pub fn project_visible(mut self, visible: bool) -> Self {
        self.project_visible = visible;
        self
    }

    pub(crate) fn supports(&self, kind: HookKind) -> bool {
        self.kinds.contains_key(&kind)
    }
}

/// Per-kind metadata in the event-kind matrix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookKindSpec {
    /// The kind represented by this entry.
    pub kind: HookKind,
    /// Whether the kind can receive an empty payload.
    pub allow_empty_payload: bool,
}

impl HookKindSpec {
    /// Creates a kind matrix entry.
    pub fn new(kind: HookKind) -> Self {
        Self {
            kind,
            allow_empty_payload: true,
        }
    }

    /// Sets whether an empty event payload is accepted.
    pub fn allow_empty_payload(mut self, allow: bool) -> Self {
        self.allow_empty_payload = allow;
        self
    }
}

/// Immutable event registry used for manifest and invocation authorization.
#[derive(Clone, Debug)]
pub struct EventRegistry {
    specs: BTreeMap<String, HookEventSpec>,
    aliases: BTreeMap<String, String>,
}

impl EventRegistry {
    /// Builds a registry from event specifications.
    pub fn new(specs: Vec<HookEventSpec>) -> HookHostResult<Self> {
        let mut registry = Self {
            specs: BTreeMap::new(),
            aliases: BTreeMap::new(),
        };
        for spec in specs {
            registry.insert(spec)?;
        }
        Ok(registry)
    }

    /// Returns the canonical event specification for an event or alias.
    pub fn spec(&self, event: &str) -> Option<&HookEventSpec> {
        let canonical = self.aliases.get(event).map(String::as_str).unwrap_or(event);
        self.specs.get(canonical)
    }

    /// Returns the canonical event name for an event or legacy alias.
    pub fn canonical_event(&self, event: &str) -> Option<String> {
        if self.specs.contains_key(event) {
            Some(event.to_owned())
        } else {
            self.aliases.get(event).cloned()
        }
    }

    /// Validates a manifest against the registry and its event-kind matrix.
    pub fn validate_manifest(&self, manifest: &HookManifest) -> HookHostResult<()> {
        manifest.validate(self)
    }

    pub(crate) fn validate_definition(
        &self,
        version: u32,
        definition: &HookDefinition,
    ) -> HookHostResult<()> {
        let Some(spec) = self.spec(&definition.event) else {
            return Err(HookHostError::UnknownEvent {
                event: definition.event.clone(),
            });
        };
        if version == 2 && !is_namespaced(&definition.event) {
            return Err(HookHostError::InvalidManifest(format!(
                "v2 event {} must be namespaced",
                definition.event
            )));
        }
        if !spec.supports(definition.kind) {
            return Err(HookHostError::DisallowedKind {
                event: definition.event.clone(),
                kind: kind_name(definition.kind).to_owned(),
            });
        }
        if definition.fields.len() > MAX_FIELDS {
            return Err(HookHostError::InvalidManifest(format!(
                "hook {} has too many fields",
                definition.id
            )));
        }
        if definition.fields_explicit {
            let mut seen = BTreeSet::new();
            for field in &definition.fields {
                if !seen.insert(field.as_str()) {
                    return Err(HookHostError::InvalidManifest(format!(
                        "hook {} repeats field {}",
                        definition.id, field
                    )));
                }
                if is_forbidden_field_name(field) {
                    return Err(HookHostError::ForbiddenField {
                        field: field.clone(),
                    });
                }
                let Some(field_spec) = spec.fields.get(field) else {
                    return Err(HookHostError::UnauthorizedField {
                        event: definition.event.clone(),
                        field: field.clone(),
                    });
                };
                if field_spec.data_class.is_forbidden() {
                    return Err(HookHostError::ForbiddenField {
                        field: field.clone(),
                    });
                }
            }
        }
        validate_matcher(&definition.matcher, spec, &definition.event, &definition.id)
    }

    pub(crate) fn authorize_fields(
        &self,
        version: u32,
        definition: &HookDefinition,
        project_scoped: bool,
    ) -> HookHostResult<Vec<HookFieldSpec>> {
        let Some(spec) = self.spec(&definition.event) else {
            return Err(HookHostError::UnknownEvent {
                event: definition.event.clone(),
            });
        };
        if project_scoped && !spec.project_visible {
            return Err(HookHostError::UnauthorizedField {
                event: definition.event.clone(),
                field: "<event>".to_owned(),
            });
        }
        let names = if version == 1 || !definition.fields_explicit {
            spec.fields.keys().cloned().collect::<Vec<_>>()
        } else {
            definition.fields.clone()
        };
        let mut fields = Vec::with_capacity(names.len());
        for name in names {
            let Some(field) = spec.fields.get(&name) else {
                return Err(HookHostError::UnauthorizedField {
                    event: definition.event.clone(),
                    field: name,
                });
            };
            if field.data_class.is_forbidden() || is_forbidden_field_name(&field.name) {
                return Err(HookHostError::ForbiddenField {
                    field: field.name.clone(),
                });
            }
            if project_scoped && !field.project_visible {
                return Err(HookHostError::UnauthorizedField {
                    event: definition.event.clone(),
                    field: field.name.clone(),
                });
            }
            fields.push(field.clone());
        }
        Ok(fields)
    }

    /// Filters and validates a payload before it crosses the process boundary.
    pub fn filter_payload(
        &self,
        version: u32,
        definition: &HookDefinition,
        project_scoped: bool,
        payload: &Value,
    ) -> HookHostResult<HookPayload> {
        let fields = self.authorize_fields(version, definition, project_scoped)?;
        let object = payload.as_object().ok_or_else(|| {
            HookHostError::InvalidInvocation("hook payload must be a JSON object".to_owned())
        })?;
        let mut filtered = Map::new();
        for field in fields {
            let Some(value) = object.get(&field.name) else {
                continue;
            };
            validate_value(&field.data_class, value, &field.name)?;
            let sanitized = sanitize_value(value)?;
            filtered.insert(field.name, sanitized);
        }
        HookPayload::from_value(Value::Object(filtered))
    }

    /// Validates and applies a transform response to the prior payload.
    pub fn apply_transform(
        &self,
        version: u32,
        definition: &HookDefinition,
        project_scoped: bool,
        previous: &HookPayload,
        response: &Value,
    ) -> HookHostResult<HookPayload> {
        let fields = self.authorize_fields(version, definition, project_scoped)?;
        let output = extract_transform_object(response)?;
        let previous_object = previous.as_value().as_object().ok_or_else(|| {
            HookHostError::InvalidInvocation("payload is not an object".to_owned())
        })?;
        let mut candidate = previous_object.clone();
        for (name, value) in output {
            let Some(field) = fields.iter().find(|field| field.name == name) else {
                return Err(HookHostError::UnauthorizedField {
                    event: definition.event.clone(),
                    field: name,
                });
            };
            if !field.transformable {
                return Err(HookHostError::InvalidInvocation(format!(
                    "field {} is not transformable",
                    field.name
                )));
            }
            validate_value(&field.data_class, &value, &field.name)?;
            candidate.insert(field.name.clone(), sanitize_value(&value)?);
        }
        HookPayload::from_value(Value::Object(candidate))
    }

    /// Returns whether a value can be represented by the registry data class.
    pub fn validate_field_value(
        &self,
        data_class: &HookDataClass,
        value: &Value,
    ) -> HookHostResult<()> {
        validate_value(data_class, value, "payload")
    }

    fn insert(&mut self, spec: HookEventSpec) -> HookHostResult<()> {
        if spec.event.is_empty() || !is_namespaced(&spec.event) {
            return Err(HookHostError::InvalidManifest(format!(
                "registry event {} must be namespaced",
                spec.event
            )));
        }
        if self.specs.contains_key(&spec.event) {
            return Err(HookHostError::InvalidManifest(format!(
                "duplicate registry event {}",
                spec.event
            )));
        }
        for (kind, kind_spec) in &spec.kinds {
            if *kind != kind_spec.kind {
                return Err(HookHostError::InvalidManifest(format!(
                    "registry event {} has a mismatched kind matrix entry",
                    spec.event
                )));
            }
        }
        for field in spec.fields.values() {
            if is_forbidden_field_name(&field.name) || field.data_class.is_forbidden() {
                return Err(HookHostError::ForbiddenField {
                    field: field.name.clone(),
                });
            }
        }
        for alias in &spec.aliases {
            if self.aliases.contains_key(alias) || self.specs.contains_key(alias) {
                return Err(HookHostError::InvalidManifest(format!(
                    "duplicate registry event alias {alias}"
                )));
            }
        }
        for alias in &spec.aliases {
            self.aliases.insert(alias.clone(), spec.event.clone());
        }
        self.specs.insert(spec.event.clone(), spec);
        Ok(())
    }
}

impl Default for EventRegistry {
    fn default() -> Self {
        let mut specs = BTreeMap::new();
        for event in default_event_specs() {
            specs.insert(event.event.clone(), event);
        }
        let mut aliases = BTreeMap::new();
        for spec in specs.values() {
            for alias in &spec.aliases {
                aliases.insert(alias.clone(), spec.event.clone());
            }
        }
        Self { specs, aliases }
    }
}

fn validate_matcher(
    matcher: &HookMatcher,
    spec: &HookEventSpec,
    event: &str,
    hook_id: &str,
) -> HookHostResult<()> {
    if matcher.tools.len() > 64 || matcher.agent_levels.len() > 16 {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {hook_id} matcher exceeds collection limits"
        )));
    }
    for key in matcher.keys() {
        if !spec.matcher_keys.contains(key) {
            return Err(HookHostError::UnauthorizedMatcher {
                event: event.to_owned(),
                key: key.to_owned(),
            });
        }
    }
    if matcher
        .tools
        .iter()
        .any(|value| value.is_empty() || value.len() > 128)
        || matcher
            .extra
            .values()
            .filter_map(Value::as_str)
            .any(|value| value.is_empty() || value.len() > 128)
    {
        return Err(HookHostError::InvalidManifest(format!(
            "hook {hook_id} matcher string is outside bounds"
        )));
    }
    for value in matcher.extra.values() {
        validate_depth(value, 0).map_err(|error| {
            HookHostError::InvalidManifest(format!(
                "hook {hook_id} matcher value is invalid: {error}"
            ))
        })?;
    }
    Ok(())
}

fn extract_transform_object(response: &Value) -> HookHostResult<Map<String, Value>> {
    let object = response.as_object().ok_or_else(|| {
        HookHostError::InvalidInvocation("transform response must be a JSON object".to_owned())
    })?;
    if let Some(payload) = object.get("payload") {
        if object.len() != 1 {
            return Err(HookHostError::InvalidInvocation(
                "transform response may contain only payload".to_owned(),
            ));
        }
        return payload.as_object().cloned().ok_or_else(|| {
            HookHostError::InvalidInvocation("transform payload must be an object".to_owned())
        });
    }
    Ok(object.clone())
}

fn validate_value(data_class: &HookDataClass, value: &Value, field: &str) -> HookHostResult<()> {
    let valid = match data_class {
        HookDataClass::Any | HookDataClass::UserContent => true,
        HookDataClass::PublicString | HookDataClass::ProjectMetadata => value.is_string(),
        HookDataClass::Number => value.is_number(),
        HookDataClass::Boolean => value.is_boolean(),
        HookDataClass::Array => value.is_array(),
        HookDataClass::Object => value.is_object(),
        HookDataClass::Secret
        | HookDataClass::Capability
        | HookDataClass::Environment
        | HookDataClass::ApiKey
        | HookDataClass::ApprovalTicket
        | HookDataClass::Endpoint => false,
    };
    if !valid {
        return Err(if data_class.is_forbidden() {
            HookHostError::ForbiddenField {
                field: field.to_owned(),
            }
        } else {
            HookHostError::InvalidInvocation(format!(
                "field {field} does not match its registry data class"
            ))
        });
    }
    let bytes = serde_json::to_vec(value)
        .map_err(|error| HookHostError::Json(error.to_string()))?
        .len();
    if bytes > crate::MAX_STDOUT_BYTES {
        return Err(HookHostError::InvalidInvocation(format!(
            "field {field} exceeds payload limits"
        )));
    }
    validate_depth(value, 0)
}

fn validate_depth(value: &Value, depth: usize) -> HookHostResult<()> {
    if depth > 8 {
        return Err(HookHostError::InvalidInvocation(
            "payload nesting exceeds limits".to_owned(),
        ));
    }
    match value {
        Value::Array(values) => {
            if values.len() > 64 {
                return Err(HookHostError::InvalidInvocation(
                    "payload array exceeds limits".to_owned(),
                ));
            }
            for value in values {
                validate_depth(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            if values.len() > 128 {
                return Err(HookHostError::InvalidInvocation(
                    "payload object exceeds limits".to_owned(),
                ));
            }
            for (key, value) in values {
                if is_forbidden_field_name(key) {
                    return Err(HookHostError::ForbiddenField { field: key.clone() });
                }
                validate_depth(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn sanitize_value(value: &Value) -> HookHostResult<Value> {
    match value {
        Value::Array(values) => values
            .iter()
            .map(sanitize_value)
            .collect::<HookHostResult<Vec<_>>>()
            .map(Value::Array),
        Value::Object(values) => {
            let mut sanitized = Map::new();
            for (key, value) in values {
                if is_forbidden_field_name(key) {
                    return Err(HookHostError::ForbiddenField { field: key.clone() });
                }
                sanitized.insert(key.clone(), sanitize_value(value)?);
            }
            Ok(Value::Object(sanitized))
        }
        _ => Ok(value.clone()),
    }
}

fn kind_name(kind: HookKind) -> &'static str {
    match kind {
        HookKind::Gate => "gate",
        HookKind::Transform => "transform",
        HookKind::Observe => "observe",
    }
}

fn is_namespaced(event: &str) -> bool {
    event.contains('.') || event.contains(':')
}

fn default_event_specs() -> Vec<HookEventSpec> {
    let all_matchers = [
        "tools",
        "agent_levels",
        "source",
        "agent_id",
        "project_id",
        "operation",
    ];
    let mut result = Vec::new();
    let observe_events = [
        ("pi.supervisor.started", "supervisor_started"),
        ("pi.supervisor.stopping", "supervisor_stopping"),
        ("pi.session.published", "session_published"),
        ("pi.session.expired", "session_expired"),
        ("pi.tool.completed", "tool_completed"),
        ("pi.tool.denied", "tool_denied"),
        ("pi.agent.started", "agent_started"),
        ("pi.agent.finished", "agent_finished"),
        ("pi.message.delivered", "message_delivered"),
        ("pi.interaction.created", "interaction_created"),
        ("pi.interaction.resolved", "interaction_resolved"),
        ("pi.team.reset", "team_reset"),
    ];
    for (event, alias) in observe_events {
        result.push(default_spec(
            event,
            alias,
            &[HookKind::Observe],
            &all_matchers,
        ));
    }
    result.push(default_spec(
        "pi.tool.dispatching",
        "tool_dispatching",
        &[HookKind::Gate, HookKind::Transform],
        &all_matchers,
    ));
    result.push(default_spec(
        "pi.agent.spawning",
        "agent_spawning",
        &[HookKind::Gate, HookKind::Transform],
        &all_matchers,
    ));
    result.push(default_spec(
        "pi.message.sending",
        "message_sending",
        &[HookKind::Gate, HookKind::Transform],
        &all_matchers,
    ));
    result.push(default_spec(
        "pi.permission.resolving",
        "permission_resolving",
        &[HookKind::Gate],
        &all_matchers,
    ));
    result.push(default_spec(
        "pi.agent.launching",
        "agent_launching",
        &[HookKind::Gate],
        &all_matchers,
    ));
    result.push(default_spec(
        "pi.interaction.resolving",
        "interaction_resolving",
        &[HookKind::Transform],
        &all_matchers,
    ));
    result.extend(ui_event_specs());
    result
}

fn ui_event_specs() -> [HookEventSpec; 3] {
    [
        typed_spec(
            "pi.ui.command.submitting",
            &[HookKind::Gate, HookKind::Transform],
            &["command_name", "source", "project_id"],
            &[
                field("command_id", HookDataClass::PublicString, false),
                field("command_name", HookDataClass::PublicString, false),
                field("source", HookDataClass::PublicString, false),
                field("project_id", HookDataClass::ProjectMetadata, false),
                field("arguments", HookDataClass::UserContent, true),
            ],
        ),
        typed_spec(
            "pi.ui.command.lifecycle",
            &[HookKind::Observe],
            &["command_name", "source", "project_id", "stage"],
            &[
                field("command_id", HookDataClass::PublicString, false),
                field("command_name", HookDataClass::PublicString, false),
                field("source", HookDataClass::PublicString, false),
                field("project_id", HookDataClass::ProjectMetadata, false),
                field("stage", HookDataClass::PublicString, false),
                field("diagnostic", HookDataClass::PublicString, false),
            ],
        ),
        typed_spec(
            "pi.state.committed",
            &[HookKind::Observe],
            &["commit_source", "project_id", "scope"],
            &[
                field("revision", HookDataClass::Number, false),
                field("topics", HookDataClass::Array, false),
                field("action_count", HookDataClass::Number, false),
                field("coalesced", HookDataClass::Boolean, false),
                field("scope", HookDataClass::PublicString, false),
                field("commit_source", HookDataClass::PublicString, false),
                field("project_id", HookDataClass::ProjectMetadata, false),
            ],
        ),
    ]
}

fn typed_spec(
    event: &str,
    kinds: &[HookKind],
    matcher_keys: &[&str],
    fields: &[HookFieldSpec],
) -> HookEventSpec {
    let mut spec = HookEventSpec::new(event);
    for kind in kinds {
        spec = spec.with_kind(*kind, HookKindSpec::new(*kind));
    }
    for key in matcher_keys {
        spec = spec.with_matcher_key(*key);
    }
    for field in fields {
        spec = spec.with_field(field.clone());
    }
    spec
}

fn default_spec(
    event: &str,
    alias: &str,
    kinds: &[HookKind],
    matcher_keys: &[&str],
) -> HookEventSpec {
    let mut spec = HookEventSpec::new(event).with_alias(alias);
    for kind in kinds {
        spec = spec.with_kind(*kind, HookKindSpec::new(*kind));
    }
    for key in matcher_keys {
        spec = spec.with_matcher_key(*key);
    }
    for field in default_fields() {
        spec = spec.with_field(field);
    }
    spec
}

fn default_fields() -> Vec<HookFieldSpec> {
    vec![
        field("tool", HookDataClass::PublicString, true),
        field("agent_level", HookDataClass::Number, false),
        field("arguments", HookDataClass::UserContent, true),
        field("name", HookDataClass::PublicString, false),
        field("task", HookDataClass::UserContent, false),
        field("message", HookDataClass::UserContent, true),
        field("target", HookDataClass::PublicString, true),
        field("decision", HookDataClass::UserContent, true),
        field("reason", HookDataClass::UserContent, false),
        field("status", HookDataClass::PublicString, false),
        field("project_root", HookDataClass::ProjectMetadata, false),
        field("operation", HookDataClass::PublicString, false),
        field("duration_ms", HookDataClass::Number, false),
        field("source", HookDataClass::PublicString, false),
        field("agent_id", HookDataClass::PublicString, false),
        field("project_id", HookDataClass::PublicString, false),
    ]
}

fn field(name: &str, data_class: HookDataClass, transformable: bool) -> HookFieldSpec {
    HookFieldSpec {
        name: name.to_owned(),
        data_class,
        transformable,
        project_visible: true,
    }
}
