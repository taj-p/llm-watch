use crate::model::{
    Event, Link, RunRecord, Snapshot, DEFAULT_RETENTION_DAYS, LINK_TTL_SECONDS, SCHEMA_VERSION,
};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub fn state_dir() -> PathBuf {
    if let Some(value) = env::var_os("LLM_WATCH_STATE_DIR") {
        return expand_home(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("XDG_STATE_HOME") {
        return expand_home(PathBuf::from(value)).join("llm-watch");
    }
    home_dir().join(".local/state/llm-watch")
}

pub fn config_dir() -> PathBuf {
    if let Some(value) = env::var_os("LLM_WATCH_CONFIG_DIR") {
        return expand_home(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        return expand_home(PathBuf::from(value)).join("llm-watch");
    }
    home_dir().join(".config/llm-watch")
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn expand_home(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        path
    }
}

pub fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

pub fn safe_name(value: &str) -> String {
    let mut output = String::new();
    let mut last_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            output.push(character);
            last_dash = false;
        } else if !last_dash {
            output.push('-');
            last_dash = true;
        }
        if output.len() >= 160 {
            break;
        }
    }
    let trimmed = output.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> AppResult<T> {
    let file = File::open(path)?;
    Ok(serde_json::from_reader(file)?)
}

pub fn read_json_if_exists<T: DeserializeOwned>(path: &Path) -> Option<T> {
    read_json(path).ok()
}

pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    atomic_write(path, |writer| {
        serde_json::to_writer(&mut *writer, value)?;
        writer.write_all(b"\n")?;
        Ok(())
    })
}

/// Writes through a temporary file in the same directory and renames it into
/// place, so a reader never observes a half-written file.
pub fn atomic_write(
    path: &Path,
    fill: impl FnOnce(&mut BufWriter<std::fs::File>) -> AppResult<()>,
) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let temporary = path.with_file_name(format!(
        ".{file_name}.{}.{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut writer = BufWriter::new(file);
    if let Err(error) = fill(&mut writer) {
        drop(writer);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn with_lock<T>(root: &Path, operation: impl FnOnce() -> AppResult<T>) -> AppResult<T> {
    fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".lock"))?;
    lock.lock_exclusive()?;
    let result = operation();
    let _ = fs2::FileExt::unlock(&lock);
    result
}

/// Must be called under the state lock.
fn update_run(event: &Event, root: &Path) -> AppResult<RunRecord> {
    let run_path = root
        .join("runs")
        .join(format!("{}.json", safe_name(&event.run_id)));
    let previous: Option<RunRecord> = read_json_if_exists(&run_path);
    let mut run = RunRecord::from(event.clone());
    if let Some(previous) = previous {
        if run.task.is_empty() {
            run.task = previous.task;
        }
        if run.tmux.is_empty() {
            run.tmux = previous.tmux;
        }
        // Only a completed turn carries a message; keep it through the
        // approval and session events that follow.
        if run.message.is_empty() {
            run.message = previous.message;
        }
    }
    atomic_json(&run_path, &run)?;
    Ok(run)
}

pub fn write_event_at(event: &Event, root: &Path) -> AppResult<RunRecord> {
    let result = with_lock(root, || {
        let run = update_run(event, root)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let event_path = root
            .join("events")
            .join(format!("{nanos:020}-{}.json", event.event_id));
        atomic_json(&event_path, event)?;
        Ok(run)
    })?;
    prune(root, DEFAULT_RETENTION_DAYS);
    Ok(result)
}

pub fn write_event(event: &Event) -> AppResult<RunRecord> {
    write_event_at(event, &state_dir())
}

/// Refreshes the run's state without touching the event log. Activity
/// heartbeats fire on every tool call: appended as events they would drown the
/// feed, and pruning on each one would rescan the directory constantly.
pub fn write_activity_at(event: &Event, root: &Path) -> AppResult<RunRecord> {
    with_lock(root, || update_run(event, root))
}

pub fn write_activity(event: &Event) -> AppResult<RunRecord> {
    write_activity_at(event, &state_dir())
}

fn link_path(root: &Path, id: &str) -> PathBuf {
    root.join("links").join(format!("{}.json", safe_name(id)))
}

pub fn write_link_at(link: &Link, root: &Path) -> AppResult<()> {
    with_lock(root, || atomic_json(&link_path(root, &link.id), link))
}

pub fn write_link(link: &Link) -> AppResult<()> {
    write_link_at(link, &state_dir())
}

/// Removing a link that was never published is not an error; callers clear on
/// exit without knowing whether the set succeeded.
pub fn clear_link_at(id: &str, root: &Path) -> AppResult<()> {
    with_lock(root, || match fs::remove_file(link_path(root, id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    })
}

pub fn clear_link(id: &str) -> AppResult<()> {
    clear_link_at(id, &state_dir())
}

/// Live links only: an entry whose owner stopped refreshing is treated as gone.
fn read_live_links(root: &Path) -> AppResult<Vec<Link>> {
    let mut links = read_directory::<Link>(&root.join("links"))?;
    links.retain(|link| {
        !link.url.is_empty()
            && parse_time(&link.updated_at)
                .is_some_and(|at| (Utc::now() - at).num_seconds() <= LINK_TTL_SECONDS)
    });
    links.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(links)
}

pub fn snapshot_at(root: &Path, event_limit: usize) -> AppResult<Snapshot> {
    with_lock(root, || {
        let mut runs = read_directory::<RunRecord>(&root.join("runs"))?;
        runs.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        let links = read_live_links(root)?;

        let mut event_paths = directory_paths(&root.join("events"))?;
        event_paths.sort();
        let start = event_paths.len().saturating_sub(event_limit);
        let mut events = Vec::new();
        for path in &event_paths[start..] {
            if let Ok(event) = read_json(path) {
                events.push(event);
            }
        }
        Ok(Snapshot {
            schema_version: SCHEMA_VERSION,
            host: short_host(),
            generated_at: utc_now(),
            runs,
            links,
            events,
        })
    })
}

pub fn snapshot(event_limit: usize) -> AppResult<Snapshot> {
    snapshot_at(&state_dir(), event_limit)
}

fn read_directory<T: DeserializeOwned>(directory: &Path) -> AppResult<Vec<T>> {
    let mut values = Vec::new();
    for path in directory_paths(directory)? {
        if let Ok(value) = read_json(&path) {
            values.push(value);
        }
    }
    Ok(values)
}

fn directory_paths(directory: &Path) -> AppResult<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn prune(root: &Path, retention_days: u64) {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(retention_days * 86_400))
        .unwrap_or(UNIX_EPOCH);
    for directory in [root.join("events"), root.join("runs")] {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.modified().is_ok_and(|modified| modified < cutoff) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub fn short_host() -> String {
    if let Ok(host) = env::var("HOSTNAME") {
        if !host.is_empty() {
            return host.split('.').next().unwrap_or(&host).to_owned();
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|host| !host.is_empty())
        .map(|host| host.split('.').next().unwrap_or(&host).to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
pub fn temporary_test_dir(label: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("llm-watch-{label}-{}", Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, state: &str, task: &str) -> Event {
        message_event(id, state, task, "")
    }

    fn message_event(id: &str, state: &str, task: &str, message: &str) -> Event {
        Event {
            schema_version: SCHEMA_VERSION,
            event_id: id.to_owned(),
            provider: "codex".to_owned(),
            event: "test".to_owned(),
            session_id: "one".to_owned(),
            run_id: "codex-one".to_owned(),
            host: "dev-one".to_owned(),
            cwd: "/workspace/app".to_owned(),
            project: "app".to_owned(),
            tmux: "app:1.0".to_owned(),
            task: task.to_owned(),
            message: message.to_owned(),
            state: state.to_owned(),
            updated_at: utc_now(),
        }
    }

    #[test]
    fn ready_event_preserves_the_prompt_task() {
        let root = temporary_test_dir("state");
        write_event_at(&event("event-1", "running", "Implement the feature"), &root).unwrap();
        write_event_at(&event("event-2", "ready", ""), &root).unwrap();
        let result = snapshot_at(&root, 10).unwrap();
        assert_eq!(result.runs.len(), 1);
        assert_eq!(result.runs[0].task, "Implement the feature");
        assert_eq!(result.runs[0].state, "ready");
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["event-1", "event-2"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn activity_refreshes_the_run_but_stays_out_of_the_event_log() {
        let root = temporary_test_dir("activity");
        write_event_at(
            &message_event("event-1", "ready", "Fix the tests", "All green."),
            &root,
        )
        .unwrap();
        write_activity_at(&event("beat-1", "running", ""), &root).unwrap();
        let result = snapshot_at(&root, 10).unwrap();
        assert_eq!(result.runs.len(), 1);
        assert_eq!(result.runs[0].state, "running");
        // The heartbeat merges like any event: task and message survive.
        assert_eq!(result.runs[0].task, "Fix the tests");
        assert_eq!(result.runs[0].message, "All green.");
        // But the event feed still only carries the real lifecycle event.
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["event-1"]
        );
        let _ = fs::remove_dir_all(root);
    }

    fn link(id: &str, url: &str, updated_at: &str) -> Link {
        Link {
            schema_version: SCHEMA_VERSION,
            id: id.to_owned(),
            kind: "difit".to_owned(),
            url: url.to_owned(),
            title: "canva".to_owned(),
            cwd: "/home/coder/work/canva".to_owned(),
            project: "canva".to_owned(),
            tmux: "main:1.1".to_owned(),
            updated_at: updated_at.to_owned(),
        }
    }

    #[test]
    fn links_expire_once_they_stop_being_refreshed() {
        let root = temporary_test_dir("links");
        let stale = (Utc::now() - Duration::from_secs(LINK_TTL_SECONDS as u64 + 60))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        write_link_at(&link("fresh", "https://a.example.dev", &utc_now()), &root).unwrap();
        write_link_at(&link("stale", "https://b.example.dev", &stale), &root).unwrap();
        // A record with no timestamp at all must not be treated as live.
        write_link_at(&link("undated", "https://c.example.dev", ""), &root).unwrap();

        let live = snapshot_at(&root, 0).unwrap().links;
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "fresh");

        // Clearing is idempotent, so an exit trap can always run it.
        clear_link_at("fresh", &root).unwrap();
        clear_link_at("fresh", &root).unwrap();
        assert!(snapshot_at(&root, 0).unwrap().links.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_last_message_survives_later_events_that_carry_none() {
        let root = temporary_test_dir("message");
        write_event_at(
            &message_event(
                "event-1",
                "ready",
                "Rebase the train",
                "Rebased and force pushed.",
            ),
            &root,
        )
        .unwrap();
        // An approval prompt arrives with no message of its own.
        write_event_at(&message_event("event-2", "approval", "", ""), &root).unwrap();
        let result = snapshot_at(&root, 10).unwrap();
        assert_eq!(result.runs[0].state, "approval");
        assert_eq!(result.runs[0].message, "Rebased and force pushed.");

        // A newer turn replaces it rather than being ignored.
        write_event_at(&message_event("event-3", "ready", "", "Tests pass."), &root).unwrap();
        let result = snapshot_at(&root, 10).unwrap();
        assert_eq!(result.runs[0].message, "Tests pass.");
        let _ = fs::remove_dir_all(root);
    }
}
