use crate::hooks::compact_text;
use crate::model::{DashboardCache, Snapshot};
use crate::storage::{atomic_json, parse_time, read_json_if_exists, state_dir, utc_now, AppResult};
use chrono::Utc;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;
use wait_timeout::ChildExt;

pub fn fetch_all(
    hosts: &[String],
    timeout: Duration,
    event_limit: usize,
) -> (BTreeMap<String, Snapshot>, BTreeMap<String, String>) {
    let handles = hosts
        .iter()
        .map(|host| {
            let host = host.clone();
            thread::spawn(move || fetch_snapshot(&host, timeout, event_limit))
        })
        .collect::<Vec<_>>();
    let mut snapshots = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for handle in handles {
        match handle.join() {
            Ok((host, Ok(snapshot))) => {
                snapshots.insert(host, snapshot);
            }
            Ok((host, Err(error))) => {
                errors.insert(host, error);
            }
            Err(_) => {}
        }
    }
    (snapshots, errors)
}

fn fetch_snapshot(
    host: &str,
    timeout: Duration,
    event_limit: usize,
) -> (String, Result<Snapshot, String>) {
    let mut command = Command::new("ssh");
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        &format!("ConnectTimeout={}", timeout.as_secs().max(1)),
        host,
        &format!("~/.local/bin/llm-watch snapshot --events {event_limit}"),
    ]);
    let result = match output_with_timeout(&mut command, timeout + Duration::from_secs(2)) {
        Ok(output) if output.status.success() => parse_snapshot_output(&output.stdout),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("SSH command failed");
            Err(message.to_owned())
        }
        Err(error) => Err(error),
    };
    (host.to_owned(), result)
}

fn parse_snapshot_output(output: &[u8]) -> Result<Snapshot, String> {
    let text = String::from_utf8_lossy(output);
    for line in text.lines().rev() {
        if let Ok(snapshot) = serde_json::from_str(line) {
            return Ok(snapshot);
        }
    }
    Err("invalid snapshot response".to_owned())
}

fn output_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("could not capture stdout")?;
    let stderr = child.stderr.take().ok_or("could not capture stderr")?;
    let stdout_reader = thread::spawn(move || {
        let mut output = Vec::new();
        let mut stream = stdout;
        stream.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = thread::spawn(move || {
        let mut output = Vec::new();
        let mut stream = stderr;
        stream.read_to_end(&mut output).map(|_| output)
    });
    let status = match child
        .wait_timeout(timeout)
        .map_err(|error| error.to_string())?
    {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("SSH timed out".to_owned());
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "stdout reader panicked".to_owned())?
        .map_err(|error| error.to_string())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "stderr reader panicked".to_owned())?
        .map_err(|error| error.to_string())?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn cache_path() -> PathBuf {
    state_dir().join("dashboard-cache.json")
}

fn load_cache() -> DashboardCache {
    read_json_if_exists(&cache_path()).unwrap_or_default()
}

fn save_cache(cache: &DashboardCache) -> AppResult<()> {
    atomic_json(&cache_path(), cache)
}

pub fn process_notifications(
    snapshots: &BTreeMap<String, Snapshot>,
    enabled: bool,
) -> AppResult<()> {
    let mut cache = load_cache();
    let mut initialized = cache
        .initialized_hosts
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let mut seen_order = cache.seen_events.clone();
    let mut seen = seen_order.iter().cloned().collect::<HashSet<_>>();

    for (host, snapshot) in snapshots {
        let current = snapshot
            .runs
            .iter()
            .map(|run| (run.run_id.as_str(), run.state.as_str()))
            .collect::<HashMap<_, _>>();
        if !initialized.contains(host) {
            for event in &snapshot.events {
                if seen.insert(event.event_id.clone()) {
                    seen_order.push(event.event_id.clone());
                }
            }
            initialized.insert(host.clone());
            continue;
        }
        for event in &snapshot.events {
            if !seen.insert(event.event_id.clone()) {
                continue;
            }
            seen_order.push(event.event_id.clone());
            if !matches!(event.state.as_str(), "ready" | "approval" | "error")
                || current.get(event.run_id.as_str()).copied() != Some(event.state.as_str())
            {
                continue;
            }
            if enabled {
                let tool = capitalize(&event.provider);
                let task = if !event.task.is_empty() {
                    &event.task
                } else if !event.project.is_empty() {
                    &event.project
                } else {
                    "session"
                };
                notify(
                    &format!("LLM Watch: {}", event.state.to_ascii_uppercase()),
                    &format!("{host} · {tool} · {task}"),
                );
            }
        }
    }

    cache.initialized_hosts = initialized.into_iter().collect();
    cache.initialized_hosts.sort();
    if seen_order.len() > 5000 {
        seen_order.drain(..seen_order.len() - 5000);
    }
    cache.seen_events = seen_order;
    cache.last_snapshots.extend(snapshots.clone());
    save_cache(&cache)
}

pub fn with_last_known(
    snapshots: &BTreeMap<String, Snapshot>,
    hosts: &[String],
) -> BTreeMap<String, Snapshot> {
    let cache = load_cache();
    let mut combined = BTreeMap::new();
    for host in hosts {
        if let Some(snapshot) = cache.last_snapshots.get(host) {
            combined.insert(host.clone(), snapshot.clone());
        }
    }
    combined.extend(snapshots.clone());
    combined
}

pub fn render_table(
    snapshots: &BTreeMap<String, Snapshot>,
    errors: &BTreeMap<String, String>,
    show_stopped: bool,
) -> String {
    let headers = ["HOST", "TMUX", "TOOL", "PROJECT", "TASK", "STATE", "AGE"];
    let mut rows = Vec::<[String; 7]>::new();
    for (alias, snapshot) in snapshots {
        for run in &snapshot.runs {
            if run.state == "stopped" && !show_stopped {
                continue;
            }
            rows.push([
                alias.clone(),
                or_dash(&run.tmux),
                or_dash(&run.provider),
                or_dash(&run.project),
                compact_text(if run.task.is_empty() { "-" } else { &run.task }, 54),
                run.state.to_ascii_uppercase(),
                age(&run.updated_at),
            ]);
        }
    }
    for (alias, message) in errors {
        rows.push([
            alias.clone(),
            "-".to_owned(),
            "-".to_owned(),
            "-".to_owned(),
            compact_text(message, 54),
            "UNREACHABLE".to_owned(),
            "-".to_owned(),
        ]);
    }
    if rows.is_empty() {
        return "No active LLM sessions found.".to_owned();
    }
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(value.chars().count());
        }
    }
    let mut lines = Vec::new();
    lines.push(join_padded(&headers.map(str::to_owned), &widths));
    lines.push(
        widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("  "),
    );
    for row in rows {
        lines.push(join_padded(&row, &widths));
    }
    lines.join("\n")
}

fn join_padded(values: &[String; 7], widths: &[usize; 7]) -> String {
    values
        .iter()
        .zip(widths)
        .map(|(value, width)| format!("{value:<width$}"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn or_dash(value: &str) -> String {
    if value.is_empty() {
        "-".to_owned()
    } else {
        value.to_owned()
    }
}

fn age(value: &str) -> String {
    let Some(timestamp) = parse_time(value) else {
        return "?".to_owned();
    };
    let seconds = (Utc::now() - timestamp).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => "LLM".to_owned(),
    }
}

fn notify(title: &str, message: &str) {
    if let Ok(configured) = env::var("LLM_WATCH_NOTIFY_COMMAND") {
        if let Ok(mut arguments) = shell_words::split(&configured) {
            if !arguments.is_empty() {
                let program = arguments.remove(0);
                let _ = Command::new(program)
                    .args(arguments)
                    .args([title, message])
                    .status();
            }
        }
    } else if command_exists("terminal-notifier") {
        let _ = Command::new("terminal-notifier")
            .args(["-title", title, "-message", message, "-group", "llm-watch"])
            .status();
    } else if cfg!(target_os = "macos") && command_exists("osascript") {
        let script = "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run";
        let _ = Command::new("osascript")
            .args(["-e", script, "--", title, message])
            .status();
    } else if command_exists("notify-send") {
        let _ = Command::new("notify-send").args([title, message]).status();
    }
}

fn command_exists(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|directory| directory.join(command).is_file()))
        .unwrap_or(false)
}

pub fn status_line() -> String {
    format!("Updated {} · Ctrl+C to stop", utc_now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RunRecord, SCHEMA_VERSION};

    #[test]
    fn remote_banner_is_ignored() {
        let snapshot = Snapshot {
            schema_version: 1,
            host: "dev".into(),
            generated_at: utc_now(),
            runs: vec![],
            events: vec![],
        };
        let output = format!("Welcome\n{}\n", serde_json::to_string(&snapshot).unwrap());
        assert_eq!(parse_snapshot_output(output.as_bytes()).unwrap(), snapshot);
    }

    #[test]
    fn stopped_rows_are_hidden() {
        let snapshot = Snapshot {
            schema_version: SCHEMA_VERSION,
            host: "dev".into(),
            generated_at: utc_now(),
            runs: vec![RunRecord {
                state: "stopped".into(),
                ..RunRecord::default()
            }],
            events: vec![],
        };
        let table = render_table(
            &BTreeMap::from([("coder.dev".into(), snapshot)]),
            &BTreeMap::new(),
            false,
        );
        assert_eq!(table, "No active LLM sessions found.");
    }
}
