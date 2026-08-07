use crate::model::{Event, MESSAGE_LIMIT, SCHEMA_VERSION, TASK_LIMIT};
use crate::storage::{short_host, utc_now, AppResult};
use serde_json::{Map, Value};
use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;
use uuid::Uuid;

/// Only the tail of a transcript is scanned; they grow to many megabytes and a
/// hook must stay well inside its timeout.
const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

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
    let activity = is_activity(&event);
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
    if task.is_empty() && state == "running" && !activity {
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

    // An activity heartbeat mid-turn has no finished answer to show, and paying
    // the transcript read on every tool call would not be worth it anyway.
    let message = if activity {
        String::new()
    } else {
        last_assistant_message(payload)
    };

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
        message,
        state,
        updated_at: utc_now(),
    })
}

/// Codex puts the text straight in the notify payload; Claude only gives a path
/// to the transcript, so the last assistant turn is read back out of it.
fn last_assistant_message(payload: &Map<String, Value>) -> String {
    let direct = string_field(
        payload,
        &[
            "last-assistant-message",
            "last_assistant_message",
            "lastAssistantMessage",
        ],
    )
    .unwrap_or_default();
    if !direct.is_empty() {
        return compact_text(&direct, MESSAGE_LIMIT);
    }
    string_field(payload, &["transcript_path", "transcript-path"])
        .map(|path| message_from_transcript(Path::new(&path)))
        .unwrap_or_default()
}

fn message_from_transcript(path: &Path) -> String {
    let Some(tail) = read_tail(path, TRANSCRIPT_TAIL_BYTES) else {
        return String::new();
    };
    // The first line may be a fragment of a record split by the tail boundary.
    for line in tail.lines().rev() {
        let Ok(Value::Object(record)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(text) = assistant_text(&record) {
            return compact_text(&text, MESSAGE_LIMIT);
        }
    }
    String::new()
}

/// Recognises a Claude transcript entry (`type: "assistant"`, content blocks)
/// and a Codex rollout entry (`payload.type: "agent_message"`).
fn assistant_text(record: &Map<String, Value>) -> Option<String> {
    if let Some(Value::Object(inner)) = record.get("payload") {
        if inner.get("type").and_then(Value::as_str) == Some("agent_message") {
            let text = inner.get("message").and_then(Value::as_str)?;
            return (!text.trim().is_empty()).then(|| text.to_owned());
        }
    }
    if record.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let content = record.get("message")?.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.trim().is_empty()).then_some(text)
}

fn read_tail(path: &Path, limit: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length > limit {
        file.seek(SeekFrom::Start(length - limit)).ok()?;
    }
    let mut buffer = Vec::with_capacity(limit.min(length) as usize);
    file.take(limit).read_to_end(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// Tool-use heartbeats: they prove the agent is working (including turns
/// resumed by background tasks, which never fire UserPromptSubmit), but fire
/// far too often to append to the event log.
pub fn is_activity(event: &str) -> bool {
    matches!(
        normalized_name(event).as_str(),
        "posttooluse" | "post-tool-use"
    )
}

fn normalize_state(event: &str, payload: &Map<String, Value>) -> Option<String> {
    let normalized = normalized_name(event);
    let state = match normalized.as_str() {
        "userpromptsubmit" | "user-prompt-submit" => "running",
        "posttooluse" | "post-tool-use" => "running",
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

pub fn tmux_target() -> String {
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
    use std::fs;

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
    fn codex_notify_payload_supplies_the_last_message() {
        let payload = object(json!({
            "type": "agent-turn-complete",
            "thread-id": "thread-1",
            "cwd": "/workspace/payments",
            "last-assistant-message": "Added retries.\n\nAll 12 tests pass."
        }));
        let event = normalize_event("codex", &payload, None).unwrap();
        assert_eq!(event.message, "Added retries. All 12 tests pass.");
    }

    #[test]
    fn claude_transcript_yields_the_final_assistant_turn() {
        let root = crate::storage::temporary_test_dir("transcript");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let lines = [
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "first"}]}}),
            json!({"type": "user", "message": {"content": [{"type": "text", "text": "ignore me"}]}}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "thinking", "thinking": "hidden"},
                {"type": "text", "text": "Rebased the   train  and force pushed."}
            ]}}),
            // Trailing non-assistant records must not hide the answer above.
            json!({"type": "system", "subtype": "hook"}),
        ];
        let body = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, body).unwrap();

        let payload = object(json!({
            "hook_event_name": "Stop",
            "session_id": "claude-1",
            "cwd": "/workspace/web",
            "transcript_path": path.display().to_string()
        }));
        let event = normalize_event("claude", &payload, None).unwrap();
        assert_eq!(event.state, "ready");
        assert_eq!(event.message, "Rebased the train and force pushed.");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_or_messageless_transcript_is_not_an_error() {
        let payload = object(json!({
            "hook_event_name": "Stop",
            "session_id": "claude-2",
            "cwd": "/workspace/web",
            "transcript_path": "/nonexistent/transcript.jsonl"
        }));
        assert_eq!(
            normalize_event("claude", &payload, None).unwrap().message,
            ""
        );
    }

    #[test]
    fn a_truncated_leading_record_is_skipped() {
        let root = crate::storage::temporary_test_dir("tail");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        // Simulates the tail window slicing through the middle of a record.
        let body = format!(
            "e\": \"broken\"}}]}}}}\n{}\n",
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "intact"}]}})
        );
        fs::write(&path, body).unwrap();
        assert_eq!(message_from_transcript(&path), "intact");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tool_activity_marks_the_run_running_without_reading_the_transcript() {
        let root = crate::storage::temporary_test_dir("activity");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        fs::write(
            &path,
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "mid-turn text"}]}})
                .to_string(),
        )
        .unwrap();

        let payload = object(json!({
            "hook_event_name": "PostToolUse",
            "session_id": "claude-1",
            "cwd": "/workspace/web",
            "tool_name": "Bash",
            "transcript_path": path.display().to_string()
        }));
        let event = normalize_event("claude", &payload, None).unwrap();
        assert_eq!(event.state, "running");
        assert!(is_activity(&event.event));
        // A heartbeat must not surface a mid-turn assistant message.
        assert_eq!(event.message, "");
        // Or clobber the task the prompt recorded (empty merges with previous).
        assert_eq!(event.task, "");
        let _ = fs::remove_dir_all(root);
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
