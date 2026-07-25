//! Reading a running agent's control state.
//!
//! Which model is selected, what thinking levels it offers, the queue modes,
//! session metrics, available slash commands: five RPCs, each waiting up to 20
//! seconds. The app used to issue them inline while drawing, so a slow or wedged
//! Pi process froze the window.
//!
//! [`fetch`] takes a [`RuntimeCommander`], which is `Send + Sync`, so the whole
//! sequence can run on a worker. It returns the [`Action`]s to apply rather than
//! touching state itself, leaving the caller to apply them on whichever thread
//! owns the state.

use std::{collections::HashMap, sync::Arc};

use pi_whim_core::{
    Action, ModelOption, ProviderProfile, SessionStatus, SlashCommandInfo, ThinkingLevel,
};
use pi_whim_runtime::RuntimeCommander;
use serde_json::{Value, json};

use crate::{
    protocol::{model_option, queue_mode, session_metrics},
    providers::provider_config_key,
};

/// Provider display names, keyed by the identifier Pi knows them under.
///
/// That key is the generated `pi-whim-<uuid>` from the models.json we hand Pi,
/// not the display name — Pi reports a model's provider by config key, and
/// without this lookup a model would render with an opaque id.
pub type ProviderNames = HashMap<String, String>;

/// Build the lookup [`fetch`] needs from the configured providers.
pub fn provider_names(profiles: &[ProviderProfile]) -> ProviderNames {
    profiles
        .iter()
        .map(|profile| (provider_config_key(profile.id), profile.name.clone()))
        .collect()
}

/// Query the agent and return the actions describing what it reported.
///
/// Runs five blocking RPCs in sequence, so call this off the thread that draws.
/// A failure to read the core state is reported as [`SessionStatus::Failed`] and
/// stops the rest; the optional extras are skipped silently, since missing
/// metrics should not present as a broken session.
pub fn fetch(commander: &Arc<dyn RuntimeCommander>, providers: &ProviderNames) -> Vec<Action> {
    let mut actions = Vec::new();

    let state = match commander.command(json!({"type": "get_state"})) {
        Ok(state) => state,
        Err(error) => {
            actions.push(Action::SetSessionStatus(SessionStatus::Failed(
                error.to_string(),
            )));
            return actions;
        }
    };

    let available_models = match commander.command(json!({"type": "get_available_models"})) {
        Ok(response) => parse_models(&response, providers),
        Err(error) => {
            actions.push(Action::SetSessionStatus(SessionStatus::Failed(
                error.to_string(),
            )));
            Vec::new()
        }
    };

    let available_thinking_levels = commander
        .command(json!({"type": "get_available_thinking_levels"}))
        .ok()
        .map(|response| parse_thinking_levels(&response))
        .unwrap_or_else(default_thinking_levels);

    // Pi can report a level this model does not offer; fall back rather than
    // showing a control the agent would reject.
    let requested = state
        .get("thinkingLevel")
        .and_then(Value::as_str)
        .and_then(|level| ThinkingLevel::try_from(level).ok())
        .unwrap_or_default();
    let thinking_level = if available_thinking_levels.contains(&requested) {
        requested
    } else {
        available_thinking_levels
            .first()
            .copied()
            .unwrap_or_default()
    };

    actions.push(Action::RuntimeControlsUpdated {
        current_model: state
            .get("model")
            .and_then(|model| model_option(model, providers)),
        available_models,
        thinking_level,
        available_thinking_levels,
        auto_compaction_enabled: state
            .get("autoCompactionEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        steering_mode: state
            .get("steeringMode")
            .and_then(Value::as_str)
            .map(queue_mode)
            .unwrap_or_default(),
        follow_up_mode: state
            .get("followUpMode")
            .and_then(Value::as_str)
            .map(queue_mode)
            .unwrap_or_default(),
    });

    if let Ok(metrics) = commander.command(json!({"type": "get_session_stats"})) {
        actions.push(Action::SessionMetricsUpdated(session_metrics(&metrics)));
    }
    if let Ok(response) = commander.command(json!({"type": "get_commands"})) {
        actions.push(Action::RuntimeCommandsUpdated(parse_commands(&response)));
    }

    actions
}

fn parse_models(response: &Value, providers: &ProviderNames) -> Vec<ModelOption> {
    response
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model_option(model, providers))
        .collect()
}

fn parse_thinking_levels(response: &Value) -> Vec<ThinkingLevel> {
    let levels: Vec<ThinkingLevel> = response
        .get("levels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|level| ThinkingLevel::try_from(level).ok())
        .collect();
    if levels.is_empty() {
        default_thinking_levels()
    } else {
        levels
    }
}

/// A picker with no options cannot be rendered, so there is always at least one.
fn default_thinking_levels() -> Vec<ThinkingLevel> {
    vec![ThinkingLevel::Off]
}

fn parse_commands(response: &Value) -> Vec<SlashCommandInfo> {
    response
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            Some(SlashCommandInfo {
                name: command.get("name")?.as_str()?.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                source: command
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("command")
                    .to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_runtime::RuntimeError;
    use std::sync::Mutex;

    /// Answers with canned responses and records what was asked.
    ///
    /// The record is behind its own `Arc` so a test can read it back while
    /// `fetch` holds the commander.
    struct StubCommander {
        responses: HashMap<String, Value>,
        failing: Option<String>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    /// A stub plus a handle to what it recorded.
    struct Stub {
        commander: Arc<dyn RuntimeCommander>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl StubCommander {
        fn build(responses: &[(&str, Value)], failing: Option<&str>) -> Stub {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let commander = Arc::new(Self {
                responses: responses
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
                failing: failing.map(str::to_owned),
                asked: asked.clone(),
            });
            Stub { commander, asked }
        }

        fn with(responses: &[(&str, Value)]) -> Arc<dyn RuntimeCommander> {
            Self::build(responses, None).commander
        }

        fn failing(command: &str) -> Arc<dyn RuntimeCommander> {
            Self::build(&[], Some(command)).commander
        }
    }

    impl RuntimeCommander for StubCommander {
        fn command(&self, command: Value) -> Result<Value, RuntimeError> {
            let kind = command
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            self.asked.lock().unwrap().push(kind.clone());
            if self.failing.as_deref() == Some(kind.as_str()) {
                return Err(RuntimeError::PiUnavailable);
            }
            Ok(self.responses.get(&kind).cloned().unwrap_or(Value::Null))
        }
    }

    fn controls(actions: &[Action]) -> Option<&Action> {
        actions
            .iter()
            .find(|action| matches!(action, Action::RuntimeControlsUpdated { .. }))
    }

    #[test]
    fn a_failed_state_read_stops_the_sequence() {
        // No point asking four more questions of a process that cannot answer
        // the first.
        let commander = StubCommander::failing("get_state");
        let actions = fetch(&commander, &ProviderNames::new());

        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            Action::SetSessionStatus(SessionStatus::Failed(_))
        ));
    }

    #[test]
    fn thinking_level_falls_back_when_pi_reports_one_it_does_not_offer() {
        let commander = StubCommander::with(&[
            ("get_state", json!({"thinkingLevel": "xhigh"})),
            ("get_available_thinking_levels", json!({"levels": ["off"]})),
        ]);
        let actions = fetch(&commander, &ProviderNames::new());

        let Some(Action::RuntimeControlsUpdated { thinking_level, .. }) = controls(&actions) else {
            panic!("expected controls");
        };
        assert_eq!(*thinking_level, ThinkingLevel::Off);
    }

    #[test]
    fn a_reported_level_that_is_offered_is_kept() {
        let commander = StubCommander::with(&[
            ("get_state", json!({"thinkingLevel": "off"})),
            (
                "get_available_thinking_levels",
                json!({"levels": ["off", "xhigh"]}),
            ),
        ]);
        let actions = fetch(&commander, &ProviderNames::new());

        let Some(Action::RuntimeControlsUpdated {
            thinking_level,
            available_thinking_levels,
            ..
        }) = controls(&actions)
        else {
            panic!("expected controls");
        };
        assert_eq!(*thinking_level, ThinkingLevel::Off);
        assert_eq!(available_thinking_levels.len(), 2);
    }

    #[test]
    fn there_is_always_at_least_one_thinking_level() {
        // An empty picker cannot be rendered.
        for levels in [json!({"levels": []}), json!({}), Value::Null] {
            let commander =
                StubCommander::with(&[("get_available_thinking_levels", levels.clone())]);
            let actions = fetch(&commander, &ProviderNames::new());

            let Some(Action::RuntimeControlsUpdated {
                available_thinking_levels,
                ..
            }) = controls(&actions)
            else {
                panic!("expected controls");
            };
            assert!(!available_thinking_levels.is_empty(), "for {levels}");
        }
    }

    #[test]
    fn auto_compaction_defaults_to_on_when_unreported() {
        let commander = StubCommander::with(&[("get_state", json!({}))]);
        let actions = fetch(&commander, &ProviderNames::new());

        let Some(Action::RuntimeControlsUpdated {
            auto_compaction_enabled,
            ..
        }) = controls(&actions)
        else {
            panic!("expected controls");
        };
        assert!(*auto_compaction_enabled);
    }

    #[test]
    fn missing_extras_do_not_present_as_a_broken_session() {
        // Metrics and commands are optional; failing to read them should not
        // mark the session failed.
        let commander = StubCommander::failing("get_session_stats");
        let actions = fetch(&commander, &ProviderNames::new());

        assert!(controls(&actions).is_some());
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::SetSessionStatus(SessionStatus::Failed(_))))
        );
    }

    #[test]
    fn slash_commands_are_parsed_with_defaults_for_absent_fields() {
        let commander = StubCommander::with(&[(
            "get_commands",
            json!({"commands": [
                {"name": "compact", "description": "Compact", "source": "builtin"},
                {"name": "bare"},
                {"description": "no name, skipped"},
            ]}),
        )]);
        let actions = fetch(&commander, &ProviderNames::new());

        let Some(Action::RuntimeCommandsUpdated(commands)) = actions
            .iter()
            .find(|action| matches!(action, Action::RuntimeCommandsUpdated(_)))
        else {
            panic!("expected commands");
        };
        // The entry without a name is dropped; the bare one gets defaults.
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].name, "bare");
        assert_eq!(commands[1].source, "command");
        assert!(commands[1].description.is_empty());
    }

    #[test]
    fn every_control_query_is_issued_once() {
        let stub = StubCommander::build(&[], None);
        fetch(&stub.commander, &ProviderNames::new());

        assert_eq!(
            *stub.asked.lock().unwrap(),
            vec![
                "get_state",
                "get_available_models",
                "get_available_thinking_levels",
                "get_session_stats",
                "get_commands",
            ]
        );
    }
}
