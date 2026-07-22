//! Local metadata repositories. Pi JSONL files remain the conversation source of truth.

use std::{
    io,
    path::{Path, PathBuf},
};

use keyring::Entry;
use pi_whim_core::{
    BashPolicy, Language, Project, ProjectId, ProviderId, ProviderModel, ProviderProfile,
    ProviderProtocol, SessionId, SessionSummary,
};
use rusqlite::{Connection, params};
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppPreferences {
    pub language: Language,
    pub bash_policy: BashPolicy,
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
                updated_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS sessions_project_updated_idx ON sessions(project_id, updated_at_ms DESC);
            CREATE TABLE IF NOT EXISTS credential_environment_names (
                environment_name TEXT PRIMARY KEY NOT NULL
            );
            CREATE TABLE IF NOT EXISTS app_preferences (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                language TEXT NOT NULL,
                bash_policy TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS provider_profiles (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                base_url TEXT NOT NULL,
                protocol TEXT NOT NULL,
                models_json TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            ",
        )?;
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
        let models_json = serde_json::to_string(&profile.models).map_err(|error| {
            PersistenceError::Sql(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        self.connection.execute(
            "INSERT INTO provider_profiles (id, name, base_url, protocol, models_json, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, base_url = excluded.base_url,
             protocol = excluded.protocol, models_json = excluded.models_json,
             updated_at_ms = excluded.updated_at_ms",
            params![
                profile.id.to_string(),
                profile.name,
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
            "SELECT id, project_id, pi_path, title, preview, updated_at_ms FROM sessions WHERE project_id = ?1 ORDER BY updated_at_ms DESC",
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
            "UPDATE sessions SET title = ?1, updated_at_ms = ?2 WHERE id = ?3",
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

impl PreferencesRepository for SqliteStore {
    fn load_preferences(&self) -> Result<AppPreferences, PersistenceError> {
        self.connection
            .query_row(
                "SELECT language, bash_policy FROM app_preferences WHERE id = 1",
                [],
                |row| {
                    let language: String = row.get(0)?;
                    let bash_policy: String = row.get(1)?;
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
                    })
                },
            )
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(AppPreferences::default()),
                error => Err(error),
            })
            .map_err(Into::into)
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
        self.connection.execute(
            "INSERT INTO app_preferences (id, language, bash_policy) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET language = excluded.language, bash_policy = excluded.bash_policy",
            params![language, bash_policy],
        )?;
        Ok(())
    }
}

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
    fn preferences_round_trip() {
        let directory = tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("test.sqlite")).unwrap();
        let preferences = AppPreferences {
            language: Language::SimplifiedChinese,
            bash_policy: BashPolicy::Ask,
        };
        store.save_preferences(preferences).unwrap();
        assert_eq!(store.load_preferences().unwrap(), preferences);
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
}
