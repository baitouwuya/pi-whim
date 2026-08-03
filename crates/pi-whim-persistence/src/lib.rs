//! Local metadata repositories. Pi JSONL files remain the conversation source of truth.

mod attachments;
mod sessions;

pub use attachments::AttachmentStore;
pub use sessions::{
    content_text, persist_session_title_to_jsonl, session_summary_from_jsonl,
    session_title_context_from_jsonl,
};

use std::{
    io,
    path::{Path, PathBuf},
};

use keyring::Entry;
use pi_whim_core::{
    AgentTeamConfig, BashPolicy, Language, OneShotAiConfig, Project, ProjectId, ProviderId,
    ProviderModel, ProviderProfile, ProviderProtocol, SearchEngineKind, SearchEngineProfile,
    SessionId, SessionSummary, normalize_bash_patterns, normalize_provider_display_name,
    provider_name_key,
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("keychain error: {0}")]
    Keychain(#[from] keyring::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] io::Error),
    #[error("application support directory is unavailable")]
    AppSupportUnavailable,
    #[error("a provider named '{0}' already exists")]
    ProviderNameConflict(String),
}

pub trait ProjectRepository {
    fn list_projects(&self) -> Result<Vec<Project>, PersistenceError>;
    fn save_project(&self, project: &Project) -> Result<(), PersistenceError>;
    fn delete_project(&self, project_id: ProjectId) -> Result<(), PersistenceError>;
}

pub trait SessionRepository {
    fn list_sessions(&self, project_id: ProjectId)
    -> Result<Vec<SessionSummary>, PersistenceError>;
    fn save_session(&self, session: &SessionSummary) -> Result<(), PersistenceError>;
    fn delete_session(&self, session_id: SessionId) -> Result<(), PersistenceError>;
    fn rename_session(&self, session_id: SessionId, title: &str) -> Result<(), PersistenceError>;
    fn set_session_ai_title(
        &self,
        session_id: SessionId,
        title: &str,
    ) -> Result<(), PersistenceError>;
}

pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, PersistenceError>;
    fn set(&self, account: &str, value: &str) -> Result<(), PersistenceError>;
    fn delete(&self, account: &str) -> Result<(), PersistenceError>;
}

pub trait CredentialRepository {
    fn list_credential_environment_names(&self) -> Result<Vec<String>, PersistenceError>;
    fn save_credential_environment_name(
        &self,
        environment_name: &str,
    ) -> Result<(), PersistenceError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppPreferences {
    pub language: Language,
    pub bash_policy: BashPolicy,
    pub bash_blocked_patterns: Vec<String>,
    pub agent_team_config: AgentTeamConfig,
    pub one_shot_ai_config: OneShotAiConfig,
}

pub trait PreferencesRepository {
    fn load_preferences(&self) -> Result<AppPreferences, PersistenceError>;
    fn save_preferences(&self, preferences: AppPreferences) -> Result<(), PersistenceError>;
}

/// Persists provider metadata only. API keys are held separately in `SecretStore`.
pub trait ProviderRepository {
    fn list_provider_profiles(&self) -> Result<Vec<ProviderProfile>, PersistenceError>;
    fn save_provider_profile(&self, profile: &ProviderProfile) -> Result<(), PersistenceError>;
    fn delete_provider_profile(&self, profile_id: ProviderId) -> Result<(), PersistenceError>;
}

pub trait SearchEngineRepository {
    fn list_search_engine_profiles(&self) -> Result<Vec<SearchEngineProfile>, PersistenceError>;
    fn save_search_engine_profiles(
        &self,
        profiles: &[SearchEngineProfile],
    ) -> Result<(), PersistenceError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookAuditEntry {
    pub project_path: String,
    pub hook_id: String,
    pub event: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub output_truncated: bool,
    pub revision: String,
    pub created_at_ms: i64,
}

pub trait HookRepository {
    fn trusted_hook_fingerprint(
        &self,
        project_path: &str,
    ) -> Result<Option<String>, PersistenceError>;
    fn approve_project_hooks(
        &self,
        project_path: &str,
        fingerprint: &str,
        approved_at_ms: i64,
    ) -> Result<(), PersistenceError>;
    fn revoke_project_hooks(&self, project_path: &str) -> Result<(), PersistenceError>;
    fn append_hook_audit(&self, entry: &HookAuditEntry) -> Result<(), PersistenceError>;
    fn recent_hook_audit(
        &self,
        project_path: &str,
        limit: usize,
    ) -> Result<Vec<HookAuditEntry>, PersistenceError>;
}

pub fn hook_manifest_fingerprint(source: &[u8]) -> String {
    Sha256::digest(source)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    pub fn open_default() -> Result<Self, PersistenceError> {
        let root = dirs::data_dir()
            .ok_or(PersistenceError::AppSupportUnavailable)?
            .join("pi-whim");
        std::fs::create_dir_all(&root)?;
        Self::open(root.join("pi-whim.sqlite"))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let connection = Connection::open(path)?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn sessions_root() -> Result<PathBuf, PersistenceError> {
        let root = dirs::data_dir()
            .ok_or(PersistenceError::AppSupportUnavailable)?
            .join("pi-whim")
            .join("sessions");
        std::fs::create_dir_all(&root)?;
        Ok(root)
    }

    fn migrate(&self) -> Result<(), PersistenceError> {
        self.connection.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                pinned INTEGER NOT NULL DEFAULT 0,
                last_opened_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY NOT NULL,
                project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                pi_path TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                preview TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                manual_title TEXT,
                ai_title TEXT
            );
            CREATE INDEX IF NOT EXISTS sessions_project_updated_idx ON sessions(project_id, updated_at_ms DESC);
            CREATE TABLE IF NOT EXISTS credential_environment_names (
                environment_name TEXT PRIMARY KEY NOT NULL
            );
            CREATE TABLE IF NOT EXISTS app_preferences (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                language TEXT NOT NULL,
                bash_policy TEXT NOT NULL,
                bash_blocked_patterns_json TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE IF NOT EXISTS agent_team_preferences (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                max_depth INTEGER NOT NULL,
                max_agents_per_level_json TEXT NOT NULL,
                policy_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS one_shot_ai_preferences (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                config_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS provider_profiles (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                name_key TEXT NOT NULL,
                base_url TEXT NOT NULL,
                protocol TEXT NOT NULL,
                models_json TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS search_engine_profiles (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                base_url TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                position INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS search_engine_profiles_position_idx
                ON search_engine_profiles (position ASC, id ASC);
            CREATE TABLE IF NOT EXISTS hook_project_trust (
                project_path TEXT PRIMARY KEY NOT NULL,
                fingerprint TEXT NOT NULL,
                approved_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS hook_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_path TEXT NOT NULL,
                hook_id TEXT NOT NULL,
                event TEXT NOT NULL,
                outcome TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                output_truncated INTEGER NOT NULL DEFAULT 0,
                revision TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS hook_audit_project_created_idx
                ON hook_audit (project_path, created_at_ms DESC);
            ",
        )?;
        self.migrate_provider_names()?;
        self.migrate_bash_preferences()?;
        self.migrate_agent_team_preferences()?;
        self.migrate_session_title_sources()?;
        self.migrate_hook_audit()?;
        Ok(())
    }

    fn migrate_bash_preferences(&self) -> Result<(), PersistenceError> {
        let columns = self
            .connection
            .prepare("PRAGMA table_info(app_preferences)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns
            .iter()
            .any(|column| column == "bash_blocked_patterns_json")
        {
            self.connection.execute(
                "ALTER TABLE app_preferences ADD COLUMN bash_blocked_patterns_json TEXT NOT NULL DEFAULT '[]'",
                [],
            )?;
        }
        Ok(())
    }

    fn migrate_agent_team_preferences(&self) -> Result<(), PersistenceError> {
        let columns = self
            .connection
            .prepare("PRAGMA table_info(agent_team_preferences)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns.iter().any(|column| column == "policy_json") {
            self.connection.execute(
                "ALTER TABLE agent_team_preferences ADD COLUMN policy_json TEXT NOT NULL DEFAULT '{}'",
                [],
            )?;
        }
        Ok(())
    }

    fn migrate_session_title_sources(&self) -> Result<(), PersistenceError> {
        let columns = self
            .connection
            .prepare("PRAGMA table_info(sessions)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns.iter().any(|column| column == "manual_title") {
            self.connection
                .execute("ALTER TABLE sessions ADD COLUMN manual_title TEXT", [])?;
        }
        if !columns.iter().any(|column| column == "ai_title") {
            self.connection
                .execute("ALTER TABLE sessions ADD COLUMN ai_title TEXT", [])?;
        }
        Ok(())
    }

    fn migrate_hook_audit(&self) -> Result<(), PersistenceError> {
        let columns = self
            .connection
            .prepare("PRAGMA table_info(hook_audit)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns.iter().any(|column| column == "output_truncated") {
            self.connection.execute(
                "ALTER TABLE hook_audit ADD COLUMN output_truncated INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        Ok(())
    }

    fn migrate_provider_names(&self) -> Result<(), PersistenceError> {
        let has_name_key = self
            .connection
            .prepare("PRAGMA table_info(provider_profiles)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "name_key");
        if !has_name_key {
            self.connection
                .execute("ALTER TABLE provider_profiles ADD COLUMN name_key TEXT", [])?;
        }

        let mut statement = self.connection.prepare(
            "SELECT id, name, updated_at_ms FROM provider_profiles
             ORDER BY updated_at_ms DESC, id ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut groups = std::collections::BTreeMap::<String, Vec<(String, String, i64)>>::new();
        for (id, name, updated_at_ms) in rows {
            let display_name = normalize_provider_display_name(&name);
            let display_name = if display_name.is_empty() {
                "Provider".to_owned()
            } else {
                display_name
            };
            groups
                .entry(provider_name_key(&display_name))
                .or_default()
                .push((id, display_name, updated_at_ms));
        }

        let mut used = std::collections::BTreeSet::new();
        let mut updates = Vec::new();
        for group in groups.values() {
            if let Some((id, winner_name, _)) = group.first() {
                used.insert(provider_name_key(winner_name));
                updates.push((id.clone(), winner_name.clone()));
            }
        }
        for group in groups.values() {
            let Some((_, winner_name, _)) = group.first() else {
                continue;
            };
            let mut suffix = 2;
            for (id, _, _) in group.iter().skip(1) {
                loop {
                    let candidate = format!("{winner_name} ({suffix})");
                    suffix += 1;
                    if used.insert(provider_name_key(&candidate)) {
                        updates.push((id.clone(), candidate));
                        break;
                    }
                }
            }
        }

        let transaction = self.connection.unchecked_transaction()?;
        for (id, name) in updates {
            transaction.execute(
                "UPDATE provider_profiles SET name = ?1, name_key = ?2 WHERE id = ?3",
                params![name, provider_name_key(&name), id],
            )?;
        }
        transaction.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS provider_profiles_name_key_unique_idx
             ON provider_profiles(name_key)",
            [],
        )?;
        transaction.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS provider_profiles_name_key_required_insert
             BEFORE INSERT ON provider_profiles
             WHEN NEW.name_key IS NULL OR trim(NEW.name_key) = ''
             BEGIN SELECT RAISE(ABORT, 'provider name key is required'); END;
             CREATE TRIGGER IF NOT EXISTS provider_profiles_name_key_required_update
             BEFORE UPDATE OF name_key ON provider_profiles
             WHEN NEW.name_key IS NULL OR trim(NEW.name_key) = ''
             BEGIN SELECT RAISE(ABORT, 'provider name key is required'); END;",
        )?;
        transaction.commit()?;
        Ok(())
    }
}

impl ProviderRepository for SqliteStore {
    fn list_provider_profiles(&self) -> Result<Vec<ProviderProfile>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, base_url, protocol, models_json, updated_at_ms
             FROM provider_profiles ORDER BY name COLLATE NOCASE",
        )?;
        let profiles = statement
            .query_map([], |row| {
                let protocol = match row.get::<_, String>(3)?.as_str() {
                    "openai-responses" => ProviderProtocol::OpenAiResponses,
                    "anthropic-messages" => ProviderProtocol::AnthropicMessages,
                    "google-generative-ai" => ProviderProtocol::GoogleGenerativeAi,
                    _ => ProviderProtocol::OpenAiCompletions,
                };
                let models_json: String = row.get(4)?;
                let models =
                    serde_json::from_str::<Vec<ProviderModel>>(&models_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                Ok(ProviderProfile {
                    id: row.get::<_, String>(0)?.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    name: row.get(1)?,
                    base_url: row.get(2)?,
                    protocol,
                    models,
                    updated_at_ms: row.get(5)?,
                    has_api_key: false,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(profiles)
    }

    fn save_provider_profile(&self, profile: &ProviderProfile) -> Result<(), PersistenceError> {
        let name = normalize_provider_display_name(&profile.name);
        let name_key = provider_name_key(&name);
        let conflicting_id = self
            .connection
            .query_row(
                "SELECT id FROM provider_profiles WHERE name_key = ?1 AND id <> ?2 LIMIT 1",
                params![name_key, profile.id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if conflicting_id.is_some() {
            return Err(PersistenceError::ProviderNameConflict(name));
        }
        let models_json = serde_json::to_string(&profile.models).map_err(|error| {
            PersistenceError::Sql(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        self.connection.execute(
            "INSERT INTO provider_profiles (id, name, name_key, base_url, protocol, models_json, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, base_url = excluded.base_url,
             name_key = excluded.name_key,
             protocol = excluded.protocol, models_json = excluded.models_json,
             updated_at_ms = excluded.updated_at_ms",
            params![
                profile.id.to_string(),
                name,
                name_key,
                profile.base_url,
                profile.protocol.pi_api(),
                models_json,
                profile.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    fn delete_provider_profile(&self, profile_id: ProviderId) -> Result<(), PersistenceError> {
        self.connection.execute(
            "DELETE FROM provider_profiles WHERE id = ?1",
            [profile_id.to_string()],
        )?;
        Ok(())
    }
}

impl SearchEngineRepository for SqliteStore {
    fn list_search_engine_profiles(&self) -> Result<Vec<SearchEngineProfile>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, kind, base_url, enabled, position
             FROM search_engine_profiles ORDER BY position ASC, id ASC",
        )?;
        let profiles = statement
            .query_map([], |row| {
                let id = row.get::<_, String>(0)?.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                let kind = match row.get::<_, String>(2)?.as_str() {
                    "searxng" => SearchEngineKind::Searxng,
                    "doubao_global" => SearchEngineKind::DoubaoGlobal,
                    _ => SearchEngineKind::Searxng,
                };
                Ok(SearchEngineProfile {
                    id,
                    name: row.get(1)?,
                    kind,
                    base_url: row.get(3)?,
                    enabled: row.get(4)?,
                    position: row.get::<_, u32>(5)?,
                    // Credential state is refreshed from Keychain after load.
                    has_api_key: false,
                }
                .normalized())
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(profiles)
    }

    fn save_search_engine_profiles(
        &self,
        profiles: &[SearchEngineProfile],
    ) -> Result<(), PersistenceError> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute("DELETE FROM search_engine_profiles", [])?;
        for (position, profile) in profiles.iter().enumerate() {
            let profile = profile.clone().normalized();
            transaction.execute(
                "INSERT INTO search_engine_profiles (id, name, kind, base_url, enabled, position)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    profile.id.to_string(),
                    profile.name,
                    profile.kind.as_str(),
                    profile.base_url,
                    profile.enabled,
                    position as u32,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

impl ProjectRepository for SqliteStore {
    fn list_projects(&self) -> Result<Vec<Project>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, path, pinned, last_opened_ms FROM projects ORDER BY pinned DESC, last_opened_ms DESC",
        )?;
        let projects = statement
            .query_map([], |row| {
                Ok(Project {
                    id: row.get::<_, String>(0)?.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    pinned: row.get(3)?,
                    last_opened_ms: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(projects)
    }

    fn save_project(&self, project: &Project) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO projects (id, name, path, pinned, last_opened_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, path = excluded.path, pinned = excluded.pinned, last_opened_ms = excluded.last_opened_ms",
            params![project.id.to_string(), project.name, project.path, project.pinned, project.last_opened_ms],
        )?;
        Ok(())
    }

    fn delete_project(&self, project_id: ProjectId) -> Result<(), PersistenceError> {
        self.connection.execute(
            "DELETE FROM projects WHERE id = ?1",
            [project_id.to_string()],
        )?;
        Ok(())
    }
}

impl SessionRepository for SqliteStore {
    fn list_sessions(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<SessionSummary>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, pi_path,
                    COALESCE(NULLIF(manual_title, ''), NULLIF(ai_title, ''), title),
                    preview, updated_at_ms
             FROM sessions WHERE project_id = ?1 ORDER BY updated_at_ms DESC",
        )?;
        let sessions = statement
            .query_map([project_id.to_string()], |row| {
                let parse = |index: usize| -> Result<Uuid, rusqlite::Error> {
                    row.get::<_, String>(index)?.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            index,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                };
                Ok(SessionSummary {
                    id: parse(0)?,
                    project_id: parse(1)?,
                    pi_path: row.get(2)?,
                    title: row.get(3)?,
                    preview: row.get(4)?,
                    updated_at_ms: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    fn save_session(&self, session: &SessionSummary) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO sessions (id, project_id, pi_path, title, preview, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET pi_path = excluded.pi_path, title = excluded.title, preview = excluded.preview, updated_at_ms = excluded.updated_at_ms",
            params![session.id.to_string(), session.project_id.to_string(), session.pi_path, session.title, session.preview, session.updated_at_ms],
        )?;
        Ok(())
    }

    fn delete_session(&self, session_id: SessionId) -> Result<(), PersistenceError> {
        self.connection.execute(
            "DELETE FROM sessions WHERE id = ?1",
            [session_id.to_string()],
        )?;
        Ok(())
    }

    fn rename_session(&self, session_id: SessionId, title: &str) -> Result<(), PersistenceError> {
        self.connection.execute(
            "UPDATE sessions SET title = ?1, manual_title = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![title, now_ms(), session_id.to_string()],
        )?;
        Ok(())
    }

    fn set_session_ai_title(
        &self,
        session_id: SessionId,
        title: &str,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "UPDATE sessions SET ai_title = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![title, now_ms(), session_id.to_string()],
        )?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl CredentialRepository for SqliteStore {
    fn list_credential_environment_names(&self) -> Result<Vec<String>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT environment_name FROM credential_environment_names ORDER BY environment_name",
        )?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?)
    }

    fn save_credential_environment_name(
        &self,
        environment_name: &str,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO credential_environment_names (environment_name) VALUES (?1)",
            [environment_name],
        )?;
        Ok(())
    }
}

impl HookRepository for SqliteStore {
    fn trusted_hook_fingerprint(
        &self,
        project_path: &str,
    ) -> Result<Option<String>, PersistenceError> {
        Ok(self
            .connection
            .query_row(
                "SELECT fingerprint FROM hook_project_trust WHERE project_path = ?1",
                [project_path],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn approve_project_hooks(
        &self,
        project_path: &str,
        fingerprint: &str,
        approved_at_ms: i64,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO hook_project_trust (project_path, fingerprint, approved_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(project_path) DO UPDATE SET fingerprint = excluded.fingerprint,
             approved_at_ms = excluded.approved_at_ms",
            params![project_path, fingerprint, approved_at_ms],
        )?;
        Ok(())
    }

    fn revoke_project_hooks(&self, project_path: &str) -> Result<(), PersistenceError> {
        self.connection.execute(
            "DELETE FROM hook_project_trust WHERE project_path = ?1",
            [project_path],
        )?;
        Ok(())
    }

    fn append_hook_audit(&self, entry: &HookAuditEntry) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO hook_audit
             (project_path, hook_id, event, outcome, duration_ms, output_truncated, revision, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.project_path,
                entry.hook_id,
                entry.event,
                entry.outcome,
                entry.duration_ms,
                entry.output_truncated,
                entry.revision,
                entry.created_at_ms,
            ],
        )?;
        Ok(())
    }

    fn recent_hook_audit(
        &self,
        project_path: &str,
        limit: usize,
    ) -> Result<Vec<HookAuditEntry>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT project_path, hook_id, event, outcome, duration_ms, output_truncated, revision, created_at_ms
             FROM hook_audit WHERE project_path = ?1 ORDER BY created_at_ms DESC, id DESC LIMIT ?2",
        )?;
        Ok(statement
            .query_map(params![project_path, limit.min(500) as i64], |row| {
                Ok(HookAuditEntry {
                    project_path: row.get(0)?,
                    hook_id: row.get(1)?,
                    event: row.get(2)?,
                    outcome: row.get(3)?,
                    duration_ms: row.get(4)?,
                    output_truncated: row.get(5)?,
                    revision: row.get(6)?,
                    created_at_ms: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }
}

impl PreferencesRepository for SqliteStore {
    fn load_preferences(&self) -> Result<AppPreferences, PersistenceError> {
        let mut preferences = self
            .connection
            .query_row(
                "SELECT language, bash_policy, bash_blocked_patterns_json FROM app_preferences WHERE id = 1",
                [],
                |row| {
                    let language: String = row.get(0)?;
                    let bash_policy: String = row.get(1)?;
                    let patterns: String = row.get(2)?;
                    let bash_blocked_patterns = serde_json::from_str::<Vec<String>>(&patterns)
                        .unwrap_or_default();
                    Ok(AppPreferences {
                        language: match language.as_str() {
                            "zh-CN" => Language::SimplifiedChinese,
                            _ => Language::English,
                        },
                        bash_policy: match bash_policy.as_str() {
                            "ask" => BashPolicy::Ask,
                            "deny" => BashPolicy::Deny,
                            _ => BashPolicy::Allow,
                        },
                        bash_blocked_patterns: normalize_bash_patterns(bash_blocked_patterns),
                        agent_team_config: AgentTeamConfig::default(),
                        one_shot_ai_config: OneShotAiConfig::default(),
                    })
                },
            )
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(AppPreferences::default()),
                error => Err(error),
            })
            .map_err(PersistenceError::from)?;
        preferences.agent_team_config =
            self.connection
                .query_row(
                    "SELECT max_depth, max_agents_per_level_json, policy_json
                 FROM agent_team_preferences WHERE id = 1",
                    [],
                    |row| {
                        let max_depth = row.get::<_, u8>(0)?;
                        let limits_json = row.get::<_, String>(1)?;
                        let policy_json = row.get::<_, String>(2)?;
                        let stored = serde_json::from_str::<serde_json::Value>(&limits_json)
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    1,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                        let unified = stored
                            .as_array()
                            .and_then(|values| values.first())
                            .cloned()
                            .unwrap_or(stored);
                        let max_agents_per_level =
                            serde_json::from_value::<u16>(unified).map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    1,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                        let policy = serde_json::from_str::<serde_json::Value>(&policy_json)
                            .unwrap_or_default();
                        Ok(AgentTeamConfig {
                            max_depth,
                            max_agents_per_level,
                            default_policy: policy
                                .get("default_policy")
                                .cloned()
                                .and_then(|value| serde_json::from_value(value).ok())
                                .unwrap_or_default(),
                            presets: policy
                                .get("presets")
                                .cloned()
                                .and_then(|value| serde_json::from_value(value).ok())
                                .unwrap_or_default(),
                        }
                        .normalized())
                    },
                )
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(AgentTeamConfig::default()),
                    error => Err(error),
                })?;
        preferences.one_shot_ai_config = self
            .connection
            .query_row(
                "SELECT config_json FROM one_shot_ai_preferences WHERE id = 1",
                [],
                |row| {
                    let config_json = row.get::<_, String>(0)?;
                    Ok(serde_json::from_str::<OneShotAiConfig>(&config_json)
                        .unwrap_or_default()
                        .normalized())
                },
            )
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(OneShotAiConfig::default()),
                error => Err(error),
            })?;
        Ok(preferences)
    }

    fn save_preferences(&self, preferences: AppPreferences) -> Result<(), PersistenceError> {
        let language = match preferences.language {
            Language::English => "en-US",
            Language::SimplifiedChinese => "zh-CN",
        };
        let bash_policy = match preferences.bash_policy {
            BashPolicy::Allow => "allow",
            BashPolicy::Ask => "ask",
            BashPolicy::Deny => "deny",
        };
        let bash_blocked_patterns =
            serde_json::to_string(&normalize_bash_patterns(preferences.bash_blocked_patterns))
                .map_err(|error| {
                    PersistenceError::Sql(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
                })?;
        self.connection.execute(
            "INSERT INTO app_preferences (id, language, bash_policy, bash_blocked_patterns_json) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET language = excluded.language, bash_policy = excluded.bash_policy,
             bash_blocked_patterns_json = excluded.bash_blocked_patterns_json",
            params![language, bash_policy, bash_blocked_patterns],
        )?;
        let team_config = preferences.agent_team_config.normalized();
        let limits_json =
            serde_json::to_string(&team_config.max_agents_per_level).map_err(|error| {
                PersistenceError::Sql(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
            })?;
        let policy_json = serde_json::to_string(&serde_json::json!({
            "default_policy": team_config.default_policy,
            "presets": team_config.presets,
        }))
        .map_err(|error| {
            PersistenceError::Sql(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        self.connection.execute(
            "INSERT INTO agent_team_preferences (id, max_depth, max_agents_per_level_json, policy_json)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET max_depth = excluded.max_depth,
             max_agents_per_level_json = excluded.max_agents_per_level_json,
             policy_json = excluded.policy_json",
            params![team_config.max_depth, limits_json, policy_json],
        )?;
        let one_shot_ai_config = serde_json::to_string(
            &preferences.one_shot_ai_config.normalized(),
        )
        .map_err(|error| {
            PersistenceError::Sql(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        self.connection.execute(
            "INSERT INTO one_shot_ai_preferences (id, config_json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET config_json = excluded.config_json",
            [one_shot_ai_config],
        )?;
        Ok(())
    }
}

/// Cheap to clone: it holds only the service name, and the keychain
/// itself is the shared resource.
#[derive(Clone)]
pub struct MacosKeychainStore {
    service: String,
}

impl MacosKeychainStore {
    pub fn new() -> Self {
        Self {
            service: "dev.pi-whim.desktop".to_owned(),
        }
    }

    fn entry(&self, account: &str) -> Result<Entry, PersistenceError> {
        Ok(Entry::new(&self.service, account)?)
    }
}

impl Default for MacosKeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for MacosKeychainStore {
    fn get(&self, account: &str) -> Result<Option<String>, PersistenceError> {
        match self.entry(account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn set(&self, account: &str, value: &str) -> Result<(), PersistenceError> {
        self.entry(account)?.set_password(value)?;
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), PersistenceError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{OneShotAiTaskConfig, SESSION_TITLE_TASK_KIND};
    use tempfile::tempdir;

    #[test]
    fn migration_and_project_index_round_trip() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let project = Project {
            id: Uuid::new_v4(),
            name: "example".into(),
            path: "/tmp/example".into(),
            pinned: true,
            last_opened_ms: 42,
        };
        store.save_project(&project).unwrap();
        assert_eq!(store.list_projects().unwrap(), vec![project]);
    }

    #[test]
    fn session_titles_keep_manual_over_ai_over_indexed_fallback() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let project = Project {
            id: Uuid::new_v4(),
            name: "example".into(),
            path: "/tmp/example".into(),
            pinned: false,
            last_opened_ms: 1,
        };
        let mut session = SessionSummary {
            id: Uuid::new_v4(),
            project_id: project.id,
            pi_path: "/tmp/example/session.jsonl".into(),
            title: "First prompt".into(),
            preview: "First prompt".into(),
            updated_at_ms: 1,
        };
        store.save_project(&project).unwrap();
        store.save_session(&session).unwrap();

        store
            .set_session_ai_title(session.id, "Generated task title")
            .unwrap();
        assert_eq!(
            store.list_sessions(project.id).unwrap()[0].title,
            "Generated task title"
        );

        session.title = "Stale JSONL title".into();
        store.save_session(&session).unwrap();
        assert_eq!(
            store.list_sessions(project.id).unwrap()[0].title,
            "Generated task title"
        );

        store.rename_session(session.id, "User title").unwrap();
        store
            .set_session_ai_title(session.id, "Late generated title")
            .unwrap();
        assert_eq!(
            store.list_sessions(project.id).unwrap()[0].title,
            "User title"
        );
    }

    #[test]
    fn legacy_sessions_table_gains_title_source_columns() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("test.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY NOT NULL,
                    project_id TEXT NOT NULL,
                    pi_path TEXT NOT NULL UNIQUE,
                    title TEXT NOT NULL,
                    preview TEXT NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );",
            )
            .unwrap();

        let store = SqliteStore::open(path).unwrap();
        let columns = store
            .connection
            .prepare("PRAGMA table_info(sessions)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "manual_title"));
        assert!(columns.iter().any(|column| column == "ai_title"));
    }

    #[test]
    fn preferences_round_trip() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let mut one_shot_ai_config = OneShotAiConfig {
            max_concurrency: 7,
            queue_capacity: 128,
            timeout_secs: 30,
            ..Default::default()
        };
        one_shot_ai_config.set_task(
            SESSION_TITLE_TASK_KIND,
            OneShotAiTaskConfig {
                enabled: true,
                provider_id: Some(Uuid::new_v4()),
                model_id: Some("example-model".into()),
                max_output_tokens: 768,
                ..Default::default()
            },
        );
        let preferences = AppPreferences {
            language: Language::SimplifiedChinese,
            bash_policy: BashPolicy::Ask,
            bash_blocked_patterns: vec!["rm -rf".into(), "curl | sh".into()],
            agent_team_config: AgentTeamConfig {
                max_depth: 3,
                max_agents_per_level: 5,
                ..Default::default()
            },
            one_shot_ai_config,
        };
        store.save_preferences(preferences.clone()).unwrap();
        assert_eq!(store.load_preferences().unwrap(), preferences);
    }

    #[test]
    fn legacy_or_invalid_one_shot_preferences_use_safe_defaults() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO one_shot_ai_preferences (id, config_json) VALUES (1, ?1)",
                [r#"{"enabled":true,"max_concurrency":99,"timeout_secs":1}"#],
            )
            .unwrap();

        let config = store.load_preferences().unwrap().one_shot_ai_config;
        assert!(config.task(SESSION_TITLE_TASK_KIND).enabled);
        assert_eq!(config.max_concurrency, 16);
        assert_eq!(config.queue_capacity, 64);
        assert_eq!(config.timeout_secs, 3);

        store
            .connection
            .execute(
                "UPDATE one_shot_ai_preferences SET config_json = 'not-json' WHERE id = 1",
                [],
            )
            .unwrap();
        assert_eq!(
            store.load_preferences().unwrap().one_shot_ai_config,
            OneShotAiConfig::default()
        );
    }

    #[test]
    fn legacy_per_level_limits_migrate_to_one_shared_limit() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO agent_team_preferences (id, max_depth, max_agents_per_level_json)
                 VALUES (1, 3, '[6,7,8]')",
                [],
            )
            .unwrap();
        let preferences = store.load_preferences().unwrap();
        assert_eq!(preferences.agent_team_config.max_depth, 3);
        assert_eq!(preferences.agent_team_config.max_agents_per_level, 6);
    }

    #[test]
    fn agent_permission_policy_and_presets_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("pi-whim.sqlite")).unwrap();
        let mut preferences = AppPreferences::default();
        preferences.agent_team_config.default_policy.level =
            pi_whim_core::AgentPermissionLevel::ReadOnly;
        preferences.agent_team_config.default_policy.enabled_tools = vec!["read".into()];
        preferences.agent_team_config.presets = vec![pi_whim_core::AgentPermissionPreset {
            name: "reviewer".into(),
            policy: pi_whim_core::AgentPermissionPolicy {
                level: pi_whim_core::AgentPermissionLevel::Controlled,
                command_allowlist: vec!["git status **".into()],
                ..Default::default()
            },
        }];
        store.save_preferences(preferences).unwrap();
        let loaded = store.load_preferences().unwrap();
        assert_eq!(
            loaded.agent_team_config.default_policy.level,
            pi_whim_core::AgentPermissionLevel::ReadOnly
        );
        assert_eq!(loaded.agent_team_config.presets[0].name, "reviewer");
        assert_eq!(
            loaded.agent_team_config.presets[0].policy.command_allowlist,
            vec!["git status **"]
        );
    }

    #[test]
    fn provider_metadata_round_trips_without_a_key() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let profile = ProviderProfile {
            id: Uuid::new_v4(),
            name: "Local gateway".into(),
            base_url: "https://gateway.example/v1".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("example-model")],
            updated_at_ms: 42,
            has_api_key: true,
        };
        store.save_provider_profile(&profile).unwrap();
        let mut expected = profile;
        expected.has_api_key = false;
        assert_eq!(store.list_provider_profiles().unwrap(), vec![expected]);
    }

    #[test]
    fn search_engine_profiles_round_trip_in_priority_order() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let first = SearchEngineProfile {
            id: Uuid::new_v4(),
            name: "Primary".into(),
            kind: SearchEngineKind::DoubaoGlobal,
            base_url: "https://primary.example/".into(),
            enabled: true,
            position: 12,
            has_api_key: true,
        };
        let second = SearchEngineProfile {
            id: Uuid::new_v4(),
            name: "Fallback".into(),
            kind: SearchEngineKind::Searxng,
            base_url: "http://localhost:8080".into(),
            enabled: false,
            position: 0,
            has_api_key: false,
        };

        store
            .save_search_engine_profiles(&[second.clone(), first.clone()])
            .unwrap();
        let profiles = store.list_search_engine_profiles().unwrap();

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, second.id);
        assert_eq!(profiles[0].position, 0);
        assert_eq!(profiles[1].id, first.id);
        assert_eq!(profiles[1].position, 1);
        assert_eq!(profiles[1].base_url, "https://primary.example");
        assert_eq!(profiles[1].kind, SearchEngineKind::DoubaoGlobal);
        assert!(!profiles[1].has_api_key);
    }

    #[test]
    fn legacy_search_engine_table_loads_without_credential_metadata() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("test.sqlite");
        let id = Uuid::new_v4();
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE search_engine_profiles (
                        id TEXT PRIMARY KEY NOT NULL,
                        name TEXT NOT NULL,
                        kind TEXT NOT NULL,
                        base_url TEXT NOT NULL,
                        enabled INTEGER NOT NULL,
                        position INTEGER NOT NULL
                    );",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO search_engine_profiles
                     (id, name, kind, base_url, enabled, position)
                     VALUES (?1, 'Legacy', 'searxng', 'http://localhost:8080', 1, 0)",
                    [id.to_string()],
                )
                .unwrap();
        }

        let store = SqliteStore::open(&path).unwrap();
        let profiles = store.list_search_engine_profiles().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, id);
        assert_eq!(profiles[0].kind, SearchEngineKind::Searxng);
        assert!(!profiles[0].has_api_key);
    }

    #[test]
    fn provider_names_are_trimmed_and_unique_case_insensitively() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let profile = |id, name: &str| ProviderProfile {
            id,
            name: name.into(),
            base_url: "https://gateway.example/v1".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("example-model")],
            updated_at_ms: 42,
            has_api_key: false,
        };
        store
            .save_provider_profile(&profile(Uuid::new_v4(), "  My Gateway  "))
            .unwrap();

        let error = store
            .save_provider_profile(&profile(Uuid::new_v4(), "my gateway"))
            .unwrap_err();
        assert!(matches!(error, PersistenceError::ProviderNameConflict(_)));
        assert_eq!(
            store.list_provider_profiles().unwrap()[0].name,
            "My Gateway"
        );

        let bypass = store.connection.execute(
            "INSERT INTO provider_profiles
             (id, name, name_key, base_url, protocol, models_json, updated_at_ms)
             VALUES (?1, 'MY GATEWAY', 'my gateway', 'https://example.test',
                     'openai-completions', '[]', 1)",
            [Uuid::new_v4().to_string()],
        );
        assert!(bypass.is_err());
    }

    #[test]
    fn legacy_duplicate_provider_names_are_renamed_before_unique_index() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("test.sqlite");
        let newest = Uuid::new_v4();
        let older = Uuid::new_v4();
        let oldest = Uuid::new_v4();
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE provider_profiles (
                        id TEXT PRIMARY KEY NOT NULL,
                        name TEXT NOT NULL,
                        base_url TEXT NOT NULL,
                        protocol TEXT NOT NULL,
                        models_json TEXT NOT NULL,
                        updated_at_ms INTEGER NOT NULL
                    );",
                )
                .unwrap();
            for (id, name, updated_at_ms) in [
                (oldest, "Gateway", 1_i64),
                (older, " gateway ", 2_i64),
                (newest, "GATEWAY", 3_i64),
            ] {
                connection
                    .execute(
                        "INSERT INTO provider_profiles
                         (id, name, base_url, protocol, models_json, updated_at_ms)
                         VALUES (?1, ?2, 'https://example.test', 'openai-completions', '[]', ?3)",
                        params![id.to_string(), name, updated_at_ms],
                    )
                    .unwrap();
            }
        }

        let store = SqliteStore::open(&path).unwrap();
        let profiles = store.list_provider_profiles().unwrap();
        let names = profiles
            .iter()
            .map(|profile| profile.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"GATEWAY"));
        assert!(names.contains(&"GATEWAY (2)"));
        assert!(names.contains(&"GATEWAY (3)"));

        let newest_name = profiles
            .iter()
            .find(|profile| profile.id == newest)
            .map(|profile| profile.name.as_str());
        assert_eq!(newest_name, Some("GATEWAY"));
    }

    #[test]
    fn project_hook_trust_is_bound_to_the_manifest_fingerprint() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let fingerprint = hook_manifest_fingerprint(br#"{"version":1,"hooks":[]}"#);
        assert_eq!(fingerprint.len(), 64);
        assert_eq!(store.trusted_hook_fingerprint("/project").unwrap(), None);

        store
            .approve_project_hooks("/project", &fingerprint, 42)
            .unwrap();
        assert_eq!(
            store.trusted_hook_fingerprint("/project").unwrap(),
            Some(fingerprint)
        );
        store.revoke_project_hooks("/project").unwrap();
        assert_eq!(store.trusted_hook_fingerprint("/project").unwrap(), None);
    }

    #[test]
    fn hook_audit_is_bounded_and_contains_no_payload_column() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        for created_at_ms in 1..=3 {
            store
                .append_hook_audit(&HookAuditEntry {
                    project_path: "/project".into(),
                    hook_id: "policy".into(),
                    event: "tool_dispatching".into(),
                    outcome: "allowed".into(),
                    duration_ms: 2,
                    output_truncated: false,
                    revision: "sha256:test".into(),
                    created_at_ms,
                })
                .unwrap();
        }
        let entries = store.recent_hook_audit("/project", 2).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].created_at_ms, 3);
        let columns = store
            .connection
            .prepare("PRAGMA table_info(hook_audit)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!columns.iter().any(|column| column == "payload"));
    }
}
