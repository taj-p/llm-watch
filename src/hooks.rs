use crate::model::{Event, SCHEMA_VERSION, TASK_LIMIT};
use crate::storage::{short_host, utc_now, AppResult};
use serde_json::{Map, Value};
use std::env;
use std::path::Path;
use std::process::Command;
use uuid::Uuid;

pub fn normalize_event(
    provider: &str,
    payload: &Map<String, Value>,
    explicit_event: Option<&str>,
) -> Option<Event> {
    let provider = provider.to_ascii_lowercase();
    let event = explicit_event
        .map(str::to_owned)
        .or_else(|| string_field(payload, &["hook_event_name", "hookEventName", "type"]))
        .unwrap_or_else(|| "unknown".to_owned());
    let state = normalize_state(&event, payload)?;
    let cwd = string_field(payload, &["cwd"])
        .or_else(|| {
            env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| ".".to_owned());
    let session_id = string_field(
        payload,
        &["session_id", "session-id", "thread_id", "thread-id"],
    )
    .unwrap_or_else(|| {
        let fallback = format!(
            "{provider}|{cwd}|{}",
            env::var("TMUX_PANE").unwrap_or_default()
        );
        Uuid::new_v5(&Uuid::NAMESPACE_URL, fallback.as_bytes()).to_string()
    });
    let mut task = env::var("LLM_WATCH_TASK")
        .ok()
        .map(|value| compact_text(&value, TASK_LIMIT))
        .unwrap_or_default();
    if task.is_empty() && state == "running" {
        task = task_from_payload(
            payload,
            &["prompt", "input", "input_messages", "input-messages"],
        );
    } else if task.is_empty() && normalized_name(&event) == "agent-turn-complete" {
        task = task_from_payload(payload, &["input_messages", "input-messages"]);
    }
    let project = Path::new(&cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&cwd)
        .to_owned();

    Some(Event {
        schema_version: SCHEMA_VERSION,
        event_id: Uuid::new_v4().to_string(),
        provider: provider.clone(),
        event,
        session_id: session_id.clone(),
        run_id: format!("{provider}-{session_id}"),
        host: short_host(),
        cwd,
        project,
        tmux: tmux_target(),
        task,
        state,
        updated_at: utc_now(),
    })
}

fn normalize_state(event: &str, payload: &Map<String, Value>) -> Option<String> {
    let normalized = normalized_name(event);
    let state = match normalized.as_str() {
        "userpromptsubmit" | "user-prompt-submit" => "running",
        "agent-turn-complete" | "stop" => "ready",
        "stopfailure" | "stop-failure" => "error",
        "sessionend" | "session-end" => "stopped",
        "sessionstart" | "session-start" => {
            if string_field(payload, &["source"])
                .is_some_and(|source| source.eq_ignore_ascii_case("compact"))
            {
                return None;
            }
            "idle"
        }
        "permissionrequest" | "permission-request" => "approval",
        "notification" => {
            let notification_type =
                string_field(payload, &["notification_type", "notification-type"])
                    .unwrap_or_default();
            match notification_type.as_str() {
                "permission_prompt" => "approval",
                "idle_prompt" => "ready",
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(state.to_owned())
}

fn normalized_name(event: &str) -> String {
    event.replace('_', "-").to_ascii_lowercase()
}

fn string_field(payload: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = payload.get(*key) else {
            continue;
        };
        match value {
            Value::String(text) if !text.is_empty() => return Some(text.clone()),
            Value::Number(number) => return Some(number.to_string()),
            Value::Bool(boolean) => return Some(boolean.to_string()),
            _ => {}
        }
    }
    None
}

fn task_from_payload(payload: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = payload.get(*key) {
            let task = compact_value(value, TASK_LIMIT);
            if !task.is_empty() {
                return task;
            }
        }
    }
    String::new()
}

pub fn compact_value(value: &Value, limit: usize) -> String {
    let raw = match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::String(text) => text.clone(),
                _ => value.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("content"))
            .map(|value| match value {
                Value::String(text) => text.clone(),
                _ => value.to_string(),
            })
            .unwrap_or_default(),
        _ => value.to_string(),
    };
    compact_text(&raw, limit)
}

pub fn compact_text(value: &str, limit: usize) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let shortened = text
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    format!("{}…", shortened.trim_end())
}

fn tmux_target() -> String {
    let Ok(pane) = env::var("TMUX_PANE") else {
        return String::new();
    };
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{session_name}:#{window_index}.#{pane_index}",
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let target = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if target.is_empty() {
                pane
            } else {
                target
            }
        }
        _ => pane,
    }
}

pub fn parse_payload(raw: &str) -> AppResult<Map<String, Value>> {
    let value: Value = serde_json::from_str(raw)?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err("hook payload must be a JSON object".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn codex_completion_is_ready_and_labeled() {
        let payload = object(json!({
            "type": "agent-turn-complete",
            "thread-id": "thread-1",
            "cwd": "/workspace/payments",
            "input-messages": ["Add retries to payment requests"]
        }));
        let event = normalize_event("codex", &payload, None).unwrap();
        assert_eq!(event.state, "ready");
        assert_eq!(event.run_id, "codex-thread-1");
        assert_eq!(event.task, "Add retries to payment requests");
        assert_eq!(event.project, "payments");
    }

    #[test]
    fn claude_permission_needs_approval() {
        let payload = object(json!({
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "session_id": "claude-1",
            "cwd": "/workspace/web"
        }));
        assert_eq!(
            normalize_event("claude", &payload, None).unwrap().state,
            "approval"
        );
    }

    #[test]
    fn compaction_is_ignored() {
        let payload = object(json!({
            "hook_event_name": "SessionStart",
            "source": "compact",
            "session_id": "one"
        }));
        assert!(normalize_event("codex", &payload, None).is_none());
    }
}
