use serde_json::Value;

pub fn last_assistant_report_from_jsonl(contents: &str) -> Option<String> {
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|entry| entry.get("message").cloned())
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .filter(|message| {
            message.get("stopReason").and_then(Value::as_str) != Some("toolUse")
                && message.get("stop_reason").and_then(Value::as_str) != Some("tool_use")
        })
        .filter_map(|message| assistant_text(&message))
        .rfind(|text| !text.trim().is_empty())
}

fn assistant_text(message: &Value) -> Option<String> {
    match message.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter(|part| {
                    part.get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == "text")
                })
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            Some(text)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_ignore_thinking_tools_and_intermediate_assistant_output() {
        let history = r#"
{"type":"message","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"working"},{"type":"toolCall","name":"read"}]}}
{"type":"message","message":{"role":"toolResult","content":[{"type":"text","text":"raw result"}]}}
{"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"thinking","thinking":"hidden final"},{"type":"text","text":"first report"}]}}
{"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"last **report**"}]}}
"#;
        assert_eq!(
            last_assistant_report_from_jsonl(history).as_deref(),
            Some("last **report**")
        );
    }

    #[test]
    fn incomplete_tail_is_ignored() {
        let history = "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":\"done\"}}\n{\"type\":";
        assert_eq!(
            last_assistant_report_from_jsonl(history).as_deref(),
            Some("done")
        );
    }
}
