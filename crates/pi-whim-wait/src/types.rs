use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub(crate) const MAX_IDENTIFIER_BYTES: usize = 128;

macro_rules! string_id {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, WaitError> {
                let value = value.into();
                if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
                    return Err(WaitError::InvalidIdentifier {
                        kind: $label,
                        max_bytes: MAX_IDENTIFIER_BYTES,
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(D::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

string_id!(WaitOwnerId, "owner");
string_id!(WaitSourceId, "source");

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct WaitTaskId(Uuid);

impl WaitTaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(self) -> Uuid {
        self.0
    }

    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

impl std::str::FromStr for WaitTaskId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self::from_uuid)
    }
}

impl Default for WaitTaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for WaitTaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WaitSourceDescriptor {
    source_id: WaitSourceId,
    public_fields: BTreeSet<String>,
    matcher_fields: BTreeSet<String>,
}

impl WaitSourceDescriptor {
    pub fn new(
        source_id: WaitSourceId,
        public_fields: impl IntoIterator<Item = impl Into<String>>,
        matcher_fields: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, WaitError> {
        let public_fields = validate_field_names(public_fields)?;
        let matcher_fields = validate_field_names(matcher_fields)?;
        if let Some(field) = matcher_fields
            .iter()
            .find(|field| !public_fields.contains(*field))
        {
            return Err(WaitError::MatcherFieldNotPublic(field.clone()));
        }
        Ok(Self {
            source_id,
            public_fields,
            matcher_fields,
        })
    }

    pub fn source_id(&self) -> &WaitSourceId {
        &self.source_id
    }

    pub fn public_fields(&self) -> &BTreeSet<String> {
        &self.public_fields
    }

    pub fn matcher_fields(&self) -> &BTreeSet<String> {
        &self.matcher_fields
    }
}

impl<'de> Deserialize<'de> for WaitSourceDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireDescriptor {
            source_id: WaitSourceId,
            public_fields: BTreeSet<String>,
            matcher_fields: BTreeSet<String>,
        }

        let wire = WireDescriptor::deserialize(deserializer)?;
        Self::new(wire.source_id, wire.public_fields, wire.matcher_fields).map_err(D::Error::custom)
    }
}

fn validate_field_names(
    fields: impl IntoIterator<Item = impl Into<String>>,
) -> Result<BTreeSet<String>, WaitError> {
    fields
        .into_iter()
        .map(Into::into)
        .map(|field| {
            if field.is_empty() || field.len() > MAX_IDENTIFIER_BYTES {
                Err(WaitError::InvalidFieldName)
            } else {
                Ok(field)
            }
        })
        .collect()
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(transparent)]
pub struct WaitMatcher(BTreeMap<String, Value>);

impl WaitMatcher {
    pub fn new(fields: BTreeMap<String, Value>) -> Result<Self, WaitError> {
        for (field, value) in &fields {
            if field.is_empty() || field.len() > MAX_IDENTIFIER_BYTES {
                return Err(WaitError::InvalidFieldName);
            }
            if !is_scalar(value) {
                return Err(WaitError::NonScalarMatcher(field.clone()));
            }
        }
        Ok(Self(fields))
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn fields(&self) -> &BTreeMap<String, Value> {
        &self.0
    }

    pub(crate) fn matches(&self, payload: &serde_json::Map<String, Value>) -> bool {
        self.0
            .iter()
            .all(|(field, expected)| payload.get(field) == Some(expected))
    }
}

impl<'de> Deserialize<'de> for WaitMatcher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = BTreeMap::<String, Value>::deserialize(deserializer)?;
        Self::new(fields).map_err(D::Error::custom)
    }
}

pub(crate) fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "selection", rename_all = "snake_case")]
pub enum WaitSourceSelection {
    Any,
    Sources { source_ids: BTreeSet<WaitSourceId> },
}

impl WaitSourceSelection {
    pub fn source(source_id: WaitSourceId) -> Self {
        Self::Sources {
            source_ids: BTreeSet::from([source_id]),
        }
    }

    pub fn sources(source_ids: impl IntoIterator<Item = WaitSourceId>) -> Self {
        Self::Sources {
            source_ids: source_ids.into_iter().collect(),
        }
    }
}

pub const MAX_WAIT_CLAUSES: usize = 16;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WaitClause {
    selection: WaitSourceSelection,
    matcher: WaitMatcher,
}

impl WaitClause {
    pub fn new(selection: WaitSourceSelection, matcher: WaitMatcher) -> Result<Self, WaitError> {
        validate_selection(&selection)?;
        Ok(Self { selection, matcher })
    }

    pub fn selection(&self) -> &WaitSourceSelection {
        &self.selection
    }

    pub fn matcher(&self) -> &WaitMatcher {
        &self.matcher
    }
}

impl<'de> Deserialize<'de> for WaitClause {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireClause {
            selection: WaitSourceSelection,
            matcher: WaitMatcher,
        }

        let wire = WireClause::deserialize(deserializer)?;
        Self::new(wire.selection, wire.matcher).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WaitQuery {
    clauses: Vec<WaitClause>,
    after_sequence: Option<u64>,
}

impl WaitQuery {
    pub fn future(selection: WaitSourceSelection, matcher: WaitMatcher) -> Self {
        Self {
            clauses: vec![WaitClause { selection, matcher }],
            after_sequence: None,
        }
    }

    pub fn after(selection: WaitSourceSelection, matcher: WaitMatcher, sequence: u64) -> Self {
        Self {
            clauses: vec![WaitClause { selection, matcher }],
            after_sequence: Some(sequence),
        }
    }

    pub fn any_of(clauses: impl IntoIterator<Item = WaitClause>) -> Result<Self, WaitError> {
        Self::from_clauses(clauses, None)
    }

    pub fn any_of_after(
        clauses: impl IntoIterator<Item = WaitClause>,
        sequence: u64,
    ) -> Result<Self, WaitError> {
        Self::from_clauses(clauses, Some(sequence))
    }

    pub fn clauses(&self) -> &[WaitClause] {
        &self.clauses
    }

    pub fn after_sequence(&self) -> Option<u64> {
        self.after_sequence
    }

    fn from_clauses(
        clauses: impl IntoIterator<Item = WaitClause>,
        after_sequence: Option<u64>,
    ) -> Result<Self, WaitError> {
        let clauses = clauses.into_iter().collect::<Vec<_>>();
        if clauses.is_empty() {
            return Err(WaitError::EmptyClauses);
        }
        if clauses.len() > MAX_WAIT_CLAUSES {
            return Err(WaitError::TooManyClauses {
                max: MAX_WAIT_CLAUSES,
            });
        }
        for clause in &clauses {
            validate_selection(&clause.selection)?;
        }
        Ok(Self {
            clauses,
            after_sequence,
        })
    }
}

impl<'de> Deserialize<'de> for WaitQuery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireQuery {
            clauses: Vec<WaitClause>,
            after_sequence: Option<u64>,
        }

        let wire = WireQuery::deserialize(deserializer)?;
        Self::from_clauses(wire.clauses, wire.after_sequence).map_err(D::Error::custom)
    }
}

fn validate_selection(selection: &WaitSourceSelection) -> Result<(), WaitError> {
    if matches!(
        selection,
        WaitSourceSelection::Sources { source_ids } if source_ids.is_empty()
    ) {
        Err(WaitError::EmptySourceSelection)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WaitEvent {
    pub sequence: u64,
    pub emitted_at_ms: u64,
    pub source_id: WaitSourceId,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WaitStatus {
    Pending,
    Matched { event: WaitEvent },
    TimedOut,
    Elapsed,
    Cancelled,
    SourceClosed,
}

impl WaitStatus {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

pub const MAX_TASK_TARGET_KIND_BYTES: usize = 64;
pub const MAX_TASK_TARGET_SUMMARY_BYTES: usize = 256;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WaitTaskMetadata {
    target_kind: String,
    target_summary: String,
}

impl WaitTaskMetadata {
    pub fn new(
        target_kind: impl Into<String>,
        target_summary: impl Into<String>,
    ) -> Result<Self, WaitError> {
        let target_kind = target_kind.into();
        if target_kind.is_empty() || target_kind.len() > MAX_TASK_TARGET_KIND_BYTES {
            return Err(WaitError::InvalidTaskMetadata {
                field: "target_kind",
                min_bytes: 1,
                max_bytes: MAX_TASK_TARGET_KIND_BYTES,
            });
        }
        let target_summary = target_summary.into();
        if target_summary.len() > MAX_TASK_TARGET_SUMMARY_BYTES {
            return Err(WaitError::InvalidTaskMetadata {
                field: "target_summary",
                min_bytes: 0,
                max_bytes: MAX_TASK_TARGET_SUMMARY_BYTES,
            });
        }
        Ok(Self {
            target_kind,
            target_summary,
        })
    }

    pub fn target_kind(&self) -> &str {
        &self.target_kind
    }

    pub fn target_summary(&self) -> &str {
        &self.target_summary
    }
}

impl<'de> Deserialize<'de> for WaitTaskMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireMetadata {
            target_kind: String,
            target_summary: String,
        }

        let wire = WireMetadata::deserialize(deserializer)?;
        Self::new(wire.target_kind, wire.target_summary).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WaitTaskSnapshot {
    pub task_id: WaitTaskId,
    pub owner_id: WaitOwnerId,
    #[serde(default)]
    pub metadata: Option<WaitTaskMetadata>,
    pub status: WaitStatus,
    pub started_after_sequence: u64,
    pub started_at_ms: u64,
    pub deadline_at_ms: u64,
    pub completed_at_ms: Option<u64>,
}

pub(crate) fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_ms)
        .unwrap_or(0)
}

pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitError {
    InvalidIdentifier {
        kind: &'static str,
        max_bytes: usize,
    },
    InvalidFieldName,
    MatcherFieldNotPublic(String),
    NonScalarMatcher(String),
    UnknownMatcherField(String),
    UnknownSource(WaitSourceId),
    DuplicateSource(WaitSourceId),
    SourceClosed(WaitSourceId),
    PayloadMustBeObject,
    UnknownPayloadField(String),
    NonScalarPublishedMatcherField(String),
    EmptySourceSelection,
    EmptyClauses,
    TooManyClauses {
        max: usize,
    },
    SequenceOverflow,
    TimeoutTooLarge,
    TaskNotFound,
    InvalidTaskMetadata {
        field: &'static str,
        min_bytes: usize,
        max_bytes: usize,
    },
    OwnerTaskLimit,
    HubTaskLimit,
    HubClosed,
    CoordinatorStart(String),
}

impl fmt::Display for WaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier { kind, max_bytes } => {
                write!(formatter, "{kind} identifier must be 1..={max_bytes} bytes")
            }
            Self::InvalidFieldName => formatter.write_str("field name is invalid"),
            Self::MatcherFieldNotPublic(field) => {
                write!(formatter, "matcher field `{field}` is not public")
            }
            Self::NonScalarMatcher(field) => {
                write!(formatter, "matcher field `{field}` must be a scalar")
            }
            Self::UnknownMatcherField(field) => {
                write!(formatter, "matcher field `{field}` is not allowed")
            }
            Self::UnknownSource(source) => write!(formatter, "unknown wait source `{source}`"),
            Self::DuplicateSource(source) => {
                write!(formatter, "wait source `{source}` is already registered")
            }
            Self::SourceClosed(source) => write!(formatter, "wait source `{source}` is closed"),
            Self::PayloadMustBeObject => {
                formatter.write_str("wait event payload must be an object")
            }
            Self::UnknownPayloadField(field) => {
                write!(formatter, "wait event field `{field}` is not public")
            }
            Self::NonScalarPublishedMatcherField(field) => write!(
                formatter,
                "published matcher field `{field}` must be a scalar"
            ),
            Self::EmptySourceSelection => formatter.write_str("source selection must not be empty"),
            Self::EmptyClauses => {
                formatter.write_str("wait query must contain at least one clause")
            }
            Self::TooManyClauses { max } => {
                write!(formatter, "wait query accepts at most {max} clauses")
            }
            Self::SequenceOverflow => formatter.write_str("wait event sequence exhausted"),
            Self::TimeoutTooLarge => formatter.write_str("wait timeout exceeds the platform limit"),
            Self::TaskNotFound => formatter.write_str("wait task not found"),
            Self::InvalidTaskMetadata {
                field,
                min_bytes,
                max_bytes,
            } => write!(
                formatter,
                "task metadata {field} must be {min_bytes}..={max_bytes} bytes"
            ),
            Self::OwnerTaskLimit => formatter.write_str("owner active wait task limit reached"),
            Self::HubTaskLimit => formatter.write_str("hub active wait task limit reached"),
            Self::HubClosed => formatter.write_str("wait hub is closed"),
            Self::CoordinatorStart(message) => {
                write!(formatter, "failed to start wait coordinator: {message}")
            }
        }
    }
}

impl std::error::Error for WaitError {}
