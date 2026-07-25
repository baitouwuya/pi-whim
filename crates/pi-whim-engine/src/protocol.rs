//! Translating Pi's wire protocol into what the UI shows.
//!
//! Pi reports tool calls, results, and agent-team activity as JSON. None of it
//! is meant to be read raw: a `bash` call becomes "ran `ls -la`", a spawned
//! sub-agent becomes a line naming the agent and its result. These functions
//! are that translation, and they are pure — JSON in, display strings out — so
//! they can be tested without a process or a window.
//!
//! Strings here face the user, so they belong on this side of the boundary
//! rather than in agent-team, whose text is written for the model.

use std::collections::HashMap;

use pi_whim_core::{ModelOption, QueueMode, SessionMetrics};
use pi_whim_persistence::content_text;
use serde_json::{Value, json};

pub fn model_option(
    value: &Value,
    provider_names: &HashMap<String, String>,
) -> Option<ModelOption> {
    let provider = value.get("provider")?.as_str()?.to_owned();
    Some(ModelOption {
        provider_name: provider_names
            .get(&provider)
            .cloned()
            .or_else(|| {
                value
                    .get("providerName")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| provider.clone()),
        provider,
        id: value.get("id")?.as_str()?.into(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| value.get("id").and_then(Value::as_str).unwrap_or("model"))
            .into(),
    })
}

pub fn queue_mode(value: &str) -> QueueMode {
    match value {
        "all" => QueueMode::All,
        _ => QueueMode::OneAtATime,
    }
}

pub fn queue_mode_name(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}

pub fn session_metrics(value: &Value) -> SessionMetrics {
    let number = |key| value.get(key).and_then(Value::as_u64).unwrap_or_default();
    let cost_microusd = value
        .get("cost")
        .and_then(Value::as_f64)
        .filter(|cost| *cost >= 0.0)
        .map(|cost| (cost * 1_000_000.0).round() as u64)
        .unwrap_or_default();
    SessionMetrics {
        total_messages: number("totalMessages"),
        user_messages: number("userMessages"),
        assistant_messages: number("assistantMessages"),
        tool_calls: number("toolCalls"),
        total_tokens: value
            .get("tokens")
            .and_then(|tokens| tokens.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cost_microusd,
    }
}

pub fn tool_result_summary(
    tool_name: Option<&str>,
    content: Option<&Value>,
    is_error: bool,
) -> String {
    let text = content_text(content).unwrap_or_default();
    if !is_error
        && let Some(summary) = tool_name.and_then(|name| agent_team_tool_summary(name, &text))
    {
        return summary;
    }
    let text = compact_tool_text(&text);
    match (is_error, text.is_empty()) {
        (true, true) => "Failed".into(),
        (true, false) => format!("Failed: {text}"),
        (false, true) => "Completed".into(),
        (false, false) => text,
    }
}

pub fn tool_call_report(name: &str, arguments: Option<&Value>) -> String {
    let argument = |key| {
        arguments
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
    };
    match name {
        "bash" => {
            let command = argument("command")
                .map(compact_tool_text)
                .unwrap_or_default();
            let background = arguments
                .and_then(|value| value.get("background"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if command.is_empty() {
                "Running Bash command.".into()
            } else if background {
                format!("Starting background command: {command}")
            } else {
                format!("Running command: {command}")
            }
        }
        "list_processes" => "Listing background processes.".into(),
        "read_process" => format!(
            "Reading process {}.",
            argument("process_id").unwrap_or("unknown")
        ),
        "stop_process" => format!(
            "Stopping process {}.",
            argument("process_id").unwrap_or("unknown")
        ),
        "read" => format!("Reading {}.", argument("path").unwrap_or("file")),
        "write" => format!("Writing {}.", argument("path").unwrap_or("file")),
        "edit" => format!("Editing {}.", argument("path").unwrap_or("file")),
        "spawn_agent" => {
            let agent = argument("name").unwrap_or("subagent");
            let task = argument("task").map(compact_tool_text).unwrap_or_default();
            if task.is_empty() {
                format!("Starting {agent}.")
            } else {
                format!("Starting {agent}:\n{task}")
            }
        }
        "send_message" => {
            let target = argument("target").unwrap_or("agent");
            let message = argument("message").unwrap_or_default();
            if message.is_empty() {
                format!("Sending a message to {target}.")
            } else {
                format!("Sending to {target}:\n{message}")
            }
        }
        "wait_agent" => format!("Waiting for {}.", argument("target").unwrap_or("agent")),
        "interrupt_agent" => format!("Interrupting {}.", argument("target").unwrap_or("agent")),
        "list_agents" => "Listing visible agents.".into(),
        "read_messages" => "Reading queued messages.".into(),
        "read_session" => format!(
            "Reading session {}.",
            argument("session_id").unwrap_or("unknown")
        ),
        "list_sessions" => "Discovering retained sessions.".into(),
        "search_sessions" => {
            let query = argument("query").unwrap_or_default();
            if query.is_empty() {
                "Searching retained sessions.".into()
            } else {
                format!("Searching sessions for: {}", compact_tool_text(query))
            }
        }
        _ => "Running.".into(),
    }
}

pub fn tool_result_report(
    tool_name: Option<&str>,
    content: Option<&Value>,
    initial_report: Option<&str>,
    is_error: bool,
) -> String {
    let text = content_text(content).unwrap_or_default();
    if tool_name == Some("bash") && !is_error {
        let result = compact_tool_text(&text);
        let prefix = initial_report.unwrap_or("Bash command");
        return if result.is_empty() {
            format!("{prefix}\nCompleted.")
        } else {
            format!("{prefix}\nResult: {result}")
        };
    }
    if !is_error
        && let Some(report) =
            tool_name.and_then(|name| agent_team_tool_report(name, &text, initial_report))
    {
        return report;
    }
    if text.trim().is_empty() {
        return if is_error {
            "Failed without a reported message.".into()
        } else {
            "Completed.".into()
        };
    }
    if is_error {
        format!("Failed:\n{text}")
    } else {
        text
    }
}

fn agent_team_tool_report(name: &str, text: &str, initial_report: Option<&str>) -> Option<String> {
    let result: Value = serde_json::from_str(text).ok()?;
    match name {
        "list_processes" => {
            let processes = result.get("processes")?.as_array()?;
            let running = processes
                .iter()
                .filter(|process| process.get("status").and_then(Value::as_str) == Some("running"))
                .count();
            Some(format!(
                "{} background process(es), {running} running",
                processes.len()
            ))
        }
        "read_process" => {
            let process = result.get("process")?;
            let id = process.get("id")?.as_str()?;
            let status = process
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Some(format!("Process {id} · {status}"))
        }
        "stop_process" => {
            let id = result
                .get("process")
                .and_then(|process| process.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("process");
            Some(format!("Stopped process {id}"))
        }
        "spawn_agent" => {
            let agent = result.get("name")?.as_str()?;
            let level = result.get("level")?.as_u64()?;
            Some(format!("Created {agent} at level {level}."))
        }
        "send_message" => {
            let mut report = initial_report
                .map(str::to_owned)
                .unwrap_or_else(|| "Sending an agent message.".into());
            if result.get("delivered").and_then(Value::as_bool) == Some(true) {
                let count = result.get("count").and_then(Value::as_u64).unwrap_or(1);
                if result.get("queued").and_then(Value::as_bool) == Some(true) {
                    report.push_str("\n\nQueued for delivery when the level-0 session resumes.");
                } else {
                    report.push_str(&format!("\n\nDelivered to {count} agent(s)."));
                }
            }
            Some(report)
        }
        "list_agents" => {
            let agents = result.get("agents")?.as_array()?;
            let lines: Vec<_> = agents
                .iter()
                .filter_map(|agent| {
                    let name = agent.get("name")?.as_str()?;
                    let level = agent.get("level")?.as_u64()?;
                    let status = agent
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let session_id = agent
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    Some(format!(
                        "- {name} · level {level} · {status} · session {session_id}"
                    ))
                })
                .collect();
            Some(if lines.is_empty() {
                "No agents are visible.".into()
            } else {
                format!("Visible agents:\n{}", lines.join("\n"))
            })
        }
        "read_messages" => {
            let messages = result.get("messages")?.as_array()?;
            Some(agent_message_report(messages, "No queued messages."))
        }
        "read_session" => {
            let agent = result.get("agent")?;
            let name = agent.get("name")?.as_str()?;
            let level = agent.get("level")?.as_u64()?;
            let session_id = result.get("session_id")?.as_str()?;
            let messages = result.get("conversation")?.as_array()?;
            let selection = result.get("selection");
            let detail = selection
                .and_then(|selection| selection.get("detail"))
                .and_then(Value::as_str)
                .unwrap_or("report");
            let truncated = selection
                .and_then(|selection| selection.get("truncated"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let access = result
                .get("access")
                .and_then(|access| access.get("send_message"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(format!(
                "Session {session_id}\n{name} · level {level} · {}\n{}",
                if access {
                    "message allowed"
                } else {
                    "read-only"
                },
                if messages.is_empty() {
                    "No conversation entries.".into()
                } else {
                    format!(
                        "{} {detail} entries returned{}.",
                        messages.len(),
                        if truncated { " (truncated)" } else { "" }
                    )
                }
            ))
        }
        "list_sessions" => {
            let sessions = result.get("sessions")?.as_array()?;
            let total = result
                .get("pagination")
                .and_then(|pagination| pagination.get("total"))
                .and_then(Value::as_u64)
                .unwrap_or(sessions.len() as u64);
            let lines: Vec<_> = sessions
                .iter()
                .map(|session| {
                    let name = session
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("session");
                    let session_id = session
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let level = session.get("level").and_then(Value::as_u64).unwrap_or(0);
                    let status = session
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    format!("- {name} · level {level} · {status} · {session_id}")
                })
                .collect();
            Some(if lines.is_empty() {
                format!("No retained sessions found (0 of {total}).")
            } else {
                format!(
                    "Retained sessions ({} of {total}):\n{}",
                    lines.len(),
                    lines.join("\n")
                )
            })
        }
        "search_sessions" => {
            let matches = result.get("matches")?.as_array()?;
            let total = result
                .get("pagination")
                .and_then(|pagination| pagination.get("total"))
                .and_then(Value::as_u64)
                .unwrap_or(matches.len() as u64);
            let lines: Vec<_> = matches
                .iter()
                .map(|item| {
                    let session_id = item
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let role = item.get("role").and_then(Value::as_str).unwrap_or("entry");
                    let snippet = item.get("snippet").and_then(Value::as_str).unwrap_or("");
                    let entry_id = item
                        .get("entry_id")
                        .and_then(Value::as_str)
                        .map(|id| format!(" · entry {id}"))
                        .unwrap_or_default();
                    format!("- {session_id}{entry_id} · {role}: {snippet}")
                })
                .collect();
            Some(if lines.is_empty() {
                format!("No matches found (0 of {total}).")
            } else {
                format!(
                    "Session matches ({} of {total}):\n{}",
                    lines.len(),
                    lines.join("\n")
                )
            })
        }
        "wait_agent" => {
            let agent = result.get("agent")?;
            let agent_name = agent.get("name")?.as_str()?;
            let wait_status = result.get("wait_status")?.as_str()?;
            let mut sections = vec![match wait_status {
                "message" => format!("Received an update from {agent_name}."),
                "completed" => format!("{agent_name} finished."),
                "timeout" => format!("{agent_name} is still running."),
                _ => format!("{agent_name}: {wait_status}"),
            }];
            if let Some(messages) = result.get("messages").and_then(Value::as_array)
                && !messages.is_empty()
            {
                sections.push(format!("Messages:\n{}", agent_message_report(messages, "")));
            }
            if let Some(outcome) = result.get("outcome") {
                if let Some(output) = outcome.get("output").and_then(Value::as_str)
                    && !output.trim().is_empty()
                {
                    sections.push(format!("Returned:\n{}", output.trim()));
                }
                if let Some(error) = outcome.get("error").and_then(Value::as_str)
                    && !error.trim().is_empty()
                {
                    sections.push(format!("Error:\n{}", error.trim()));
                }
            }
            if let Some(descendants) = result.get("descendants").and_then(Value::as_array) {
                for descendant in descendants {
                    let Some(agent) = descendant.get("agent") else {
                        continue;
                    };
                    let name = agent
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("descendant");
                    let Some(outcome) = descendant.get("outcome") else {
                        continue;
                    };
                    if let Some(output) = outcome.get("output").and_then(Value::as_str)
                        && !output.trim().is_empty()
                    {
                        sections.push(format!("{name} returned:\n{}", output.trim()));
                    }
                    if let Some(error) = outcome.get("error").and_then(Value::as_str)
                        && !error.trim().is_empty()
                    {
                        sections.push(format!("{name} error:\n{}", error.trim()));
                    }
                }
            }
            Some(sections.join("\n\n"))
        }
        "interrupt_agent" => result
            .get("target")
            .and_then(Value::as_str)
            .map(|target| format!("Interrupted {target}.")),
        _ => None,
    }
}

fn agent_message_report(messages: &[Value], empty_message: &str) -> String {
    let lines: Vec<_> = messages
        .iter()
        .filter_map(|message| {
            let sender = message.get("sender_name")?.as_str()?;
            let content = message.get("content")?.as_str()?.trim();
            Some(format!("- {sender}: {content}"))
        })
        .collect();
    if lines.is_empty() {
        empty_message.into()
    } else {
        lines.join("\n")
    }
}

pub fn tool_event_details(event: &Value, previous_details: Option<&str>) -> String {
    let details = if event.get("type").and_then(Value::as_str) == Some("tool_execution_end") {
        let input = previous_details
            .and_then(|details| serde_json::from_str::<Value>(details).ok())
            .and_then(|details| {
                details
                    .get("input")
                    .cloned()
                    .or_else(|| details.get("args").cloned())
            })
            .unwrap_or(Value::Null);
        json!({
            "input": input,
            "result": event.get("result").cloned().unwrap_or(Value::Null),
            "is_error": event.get("isError").and_then(Value::as_bool).unwrap_or(false),
        })
    } else {
        event.clone()
    };
    serde_json::to_string_pretty(&details).unwrap_or_else(|_| details.to_string())
}

fn agent_team_tool_summary(name: &str, text: &str) -> Option<String> {
    if name == "bash" {
        let result = compact_tool_text(text);
        return Some(if result.is_empty() {
            "Bash command completed".into()
        } else {
            format!("Bash: {result}")
        });
    }
    let result: Value = serde_json::from_str(text).ok()?;
    match name {
        "list_processes" => {
            let processes = result.get("processes")?.as_array()?;
            let running = processes
                .iter()
                .filter(|process| process.get("status").and_then(Value::as_str) == Some("running"))
                .count();
            Some(format!(
                "{} process(es), {running} running",
                processes.len()
            ))
        }
        "read_process" => {
            let process = result.get("process")?;
            let status = process
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Some(format!("Process {status}"))
        }
        "stop_process" => Some("Process stopped".into()),
        "spawn_agent" => {
            let name = result.get("name")?.as_str()?;
            let level = result.get("level")?.as_u64()?;
            Some(format!("Started {name} (level {level})"))
        }
        "send_message" => result.get("count").and_then(Value::as_u64).map(|count| {
            if result.get("queued").and_then(Value::as_bool) == Some(true) {
                "Message queued for level-0 session".into()
            } else {
                format!("Message delivered to {count} agent(s)")
            }
        }),
        "list_agents" => result
            .get("agents")
            .and_then(Value::as_array)
            .map(|agents| format!("{} agents visible", agents.len())),
        "read_messages" => result
            .get("messages")
            .and_then(Value::as_array)
            .map(|messages| format!("{} messages received", messages.len())),
        "read_session" => {
            let agent = result.get("agent")?;
            let name = agent.get("name")?.as_str()?;
            let level = agent.get("level")?.as_u64()?;
            Some(format!("Read {name} session (level {level})"))
        }
        "list_sessions" => result
            .get("sessions")
            .and_then(Value::as_array)
            .map(|sessions| format!("{} retained sessions found", sessions.len())),
        "search_sessions" => result
            .get("matches")
            .and_then(Value::as_array)
            .map(|matches| format!("{} session matches found", matches.len())),
        "wait_agent" => {
            let agent = result.get("agent")?;
            let agent_name = agent.get("name")?.as_str()?;
            match result.get("wait_status")?.as_str()? {
                "message" => Some(format!("{agent_name} sent a message")),
                "completed" => {
                    let failed = result
                        .get("outcome")
                        .and_then(|outcome| outcome.get("error"))
                        .and_then(Value::as_str)
                        .is_some_and(|error| !error.trim().is_empty());
                    Some(if failed {
                        format!("{agent_name} failed")
                    } else {
                        format!("{agent_name} completed")
                    })
                }
                "timeout" => Some(format!("{agent_name} is still running")),
                _ => None,
            }
        }
        "interrupt_agent" => result
            .get("target")
            .and_then(Value::as_str)
            .map(|target| format!("Interrupted {target}")),
        _ => None,
    }
}

fn compact_tool_text(text: &str) -> String {
    const MAX_SUMMARY_CHARS: usize = 84;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_SUMMARY_CHARS {
        return compact;
    }
    let prefix: String = compact.chars().take(MAX_SUMMARY_CHARS).collect();
    format!("{prefix}…")
}

pub fn assistant_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            // Thinking blocks join the visible transcript wrapped in
            // `<thinking>` tags; the UI markdown renderer recognizes the tags
            // and renders the section muted instead of showing raw markup.
            let mut blocks: Vec<String> = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("thinking") => {
                        if let Some(thinking) = part.get("thinking").and_then(Value::as_str) {
                            let thinking = thinking.trim();
                            if !thinking.is_empty() {
                                blocks.push(format!("<thinking>\n{thinking}\n</thinking>"));
                            }
                        }
                    }
                    _ => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            blocks.push(text.to_owned());
                        }
                    }
                }
            }
            blocks.join("\n\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_text_wraps_thinking_blocks_in_tags() {
        let message = json!({
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "why" },
                { "type": "text", "text": "answer" }
            ]
        });
        assert_eq!(
            assistant_text(&message),
            "<thinking>\nwhy\n</thinking>\n\nanswer"
        );
    }

    #[test]
    fn assistant_text_skips_empty_thinking_and_reads_string_content() {
        let message = json!({
            "content": [
                { "type": "thinking", "thinking": "   " },
                { "type": "text", "text": "answer" }
            ]
        });
        assert_eq!(assistant_text(&message), "answer");

        let plain = json!({ "content": "plain" });
        assert_eq!(assistant_text(&plain), "plain");
    }

    #[test]
    fn agent_team_tool_results_have_a_compact_summary() {
        let result = json!({
            "agent": { "name": "worker-alpha" },
            "wait_status": "message",
        });
        let content = json!([{ "type": "text", "text": result.to_string() }]);

        assert_eq!(
            tool_result_summary(Some("wait_agent"), Some(&content), false),
            "worker-alpha sent a message"
        );
    }

    #[test]
    fn waiting_report_shows_messages_and_the_child_result_without_raw_json() {
        let result = json!({
            "agent": { "name": "worker-alpha" },
            "messages": [{ "sender_name": "worker-alpha", "content": "Need approval." }],
            "outcome": { "output": "Task complete.", "error": "" },
            "wait_status": "completed",
        });
        let report = agent_team_tool_report("wait_agent", &result.to_string(), None).unwrap();

        assert!(report.contains("worker-alpha finished."));
        assert!(report.contains("worker-alpha: Need approval."));
        assert!(report.contains("Returned:\nTask complete."));
        assert!(!report.contains("\"wait_status\""));
    }

    #[test]
    fn bash_and_process_tools_use_compact_operation_reports() {
        let args = json!({
            "command": "cargo test --workspace",
            "background": true,
        });
        let initial = tool_call_report("bash", Some(&args));
        assert_eq!(
            initial,
            "Starting background command: cargo test --workspace"
        );
        let content = json!([{ "type": "text", "text": "Background process 123 started." }]);
        let report = tool_result_report(Some("bash"), Some(&content), Some(&initial), false);
        assert!(report.contains("Starting background command"));
        assert!(report.contains("Background process 123 started."));
        assert!(!report.contains("\"command\""));

        let processes = json!({
            "processes": [{
                "id": "123",
                "status": "running"
            }]
        });
        let process_report =
            agent_team_tool_report("list_processes", &processes.to_string(), None).unwrap();
        assert_eq!(process_report, "1 background process(es), 1 running");
    }

    #[test]
    fn generic_tool_summaries_are_single_line_and_bounded() {
        assert_eq!(
            compact_tool_text("first\n second\tthird"),
            "first second third"
        );
        assert!(compact_tool_text(&"x ".repeat(200)).ends_with('…'));
    }
}
