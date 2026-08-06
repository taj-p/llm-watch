use crate::dashboard::{fetch_all, process_notifications, with_last_known};
use crate::labels::{labels_path, read_labels, sanitize, set_label};
use crate::model::{RunRecord, Snapshot, SCHEMA_VERSION};
use crate::storage::{utc_now, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("web/index.html");
const HEARTBEAT: Duration = Duration::from_secs(15);
const MAX_CONNECTIONS: usize = 64;
const MAX_REQUEST_LINE: u64 = 8 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024;

pub struct ServeOptions {
    pub hosts: Vec<String>,
    pub bind: String,
    pub port: u16,
    pub interval: Duration,
    pub timeout: Duration,
    pub events: usize,
    pub notify: bool,
    pub open: bool,
}

#[derive(Deserialize)]
struct LabelRequest {
    host: String,
    #[serde(default)]
    label: String,
}

#[derive(Clone, Serialize)]
struct StateView {
    schema_version: u32,
    generated_at: String,
    interval_seconds: f64,
    hosts: Vec<HostView>,
}

#[derive(Clone, Serialize)]
struct HostView {
    host: String,
    /// Free-text work-stream marker from the labels config.
    label: String,
    /// True when this cycle's SSH poll succeeded.
    reachable: bool,
    /// True when the rows come from the last known snapshot instead of this cycle.
    stale: bool,
    error: Option<String>,
    observed_at: Option<String>,
    runs: Vec<RunRecord>,
}

/// Latest serialized payload plus a version other threads can wait on.
struct Broadcast {
    state: Mutex<Published>,
    changed: Condvar,
}

struct Published {
    version: u64,
    payload: Arc<String>,
    /// Kept structured so a label edit can patch and republish without waiting
    /// for the next SSH poll.
    view: StateView,
}

impl Broadcast {
    fn new(view: StateView) -> AppResult<Self> {
        Ok(Self {
            state: Mutex::new(Published {
                version: 1,
                payload: Arc::new(serde_json::to_string(&view)?),
                view,
            }),
            changed: Condvar::new(),
        })
    }

    fn publish(&self, view: StateView) -> AppResult<()> {
        let payload = serde_json::to_string(&view)?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.version += 1;
        state.payload = Arc::new(payload);
        state.view = view;
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    /// Returns false when the host is not one of the watched aliases.
    fn apply_label(&self, host: &str, label: &str) -> AppResult<bool> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(entry) = state.view.hosts.iter_mut().find(|view| view.host == host) else {
            return Ok(false);
        };
        entry.label = label.to_owned();
        let payload = serde_json::to_string(&state.view)?;
        state.version += 1;
        state.payload = Arc::new(payload);
        drop(state);
        self.changed.notify_all();
        Ok(true)
    }

    fn current(&self) -> (u64, Arc<String>) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (state.version, state.payload.clone())
    }

    /// Blocks until the payload moves past `version`, or the heartbeat elapses.
    fn wait_after(&self, version: u64, timeout: Duration) -> Option<(u64, Arc<String>)> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.version != version {
            return Some((state.version, state.payload.clone()));
        }
        let (state, _) = self
            .changed
            .wait_timeout(state, timeout)
            .unwrap_or_else(|error| error.into_inner());
        if state.version != version {
            Some((state.version, state.payload.clone()))
        } else {
            None
        }
    }
}

pub fn serve(options: ServeOptions) -> AppResult<u8> {
    let listener = TcpListener::bind((options.bind.as_str(), options.port))?;
    let address = listener.local_addr()?;
    let url = format!("http://{address}/");

    let hosts = Arc::new(options.hosts);
    let interval = options.interval;
    let labels = read_labels(&labels_path());
    let broadcast = Arc::new(Broadcast::new(StateView {
        schema_version: SCHEMA_VERSION,
        generated_at: utc_now(),
        interval_seconds: interval.as_secs_f64(),
        hosts: hosts
            .iter()
            .map(|host| HostView {
                host: host.clone(),
                label: labels.get(host).cloned().unwrap_or_default(),
                reachable: false,
                stale: false,
                error: None,
                observed_at: None,
                runs: Vec::new(),
            })
            .collect(),
    })?);

    {
        let broadcast = Arc::clone(&broadcast);
        let hosts = Arc::clone(&hosts);
        let timeout = options.timeout;
        let events = options.events;
        let notify = options.notify;
        thread::Builder::new()
            .name("llm-watch-poller".to_owned())
            .spawn(move || loop {
                match poll_once(&hosts, timeout, events, notify, interval)
                    .and_then(|view| broadcast.publish(view))
                {
                    Ok(()) => {}
                    Err(error) => eprintln!("llm-watch: poll failed: {error}"),
                }
                thread::sleep(interval);
            })?;
    }

    println!(
        "llm-watch serving {} host{} on {url}",
        hosts.len(),
        if hosts.len() == 1 { "" } else { "s" }
    );
    println!("Ctrl+C to stop");
    if options.open {
        open_browser(&url);
    }

    let connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            let mut stream = stream;
            let _ = write_simple(
                &mut stream,
                "503 Service Unavailable",
                "text/plain",
                b"busy",
            );
            continue;
        }
        connections.fetch_add(1, Ordering::SeqCst);
        let broadcast = Arc::clone(&broadcast);
        let owned = Arc::clone(&connections);
        let spawned = thread::Builder::new()
            .name("llm-watch-connection".to_owned())
            .spawn(move || {
                handle_connection(stream, &broadcast);
                owned.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            connections.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(0)
}

fn poll_once(
    hosts: &[String],
    timeout: Duration,
    events: usize,
    notify: bool,
    interval: Duration,
) -> AppResult<StateView> {
    let (snapshots, errors) = fetch_all(hosts, timeout, events);
    process_notifications(&snapshots, notify)?;
    let displayed = with_last_known(&snapshots, hosts);
    // Re-read every cycle so edits to the labels file land without a restart.
    let labels = read_labels(&labels_path());
    Ok(build_state(
        hosts, &snapshots, &displayed, &errors, &labels, interval,
    ))
}

fn build_state(
    hosts: &[String],
    snapshots: &BTreeMap<String, Snapshot>,
    displayed: &BTreeMap<String, Snapshot>,
    errors: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
    interval: Duration,
) -> StateView {
    let hosts = hosts
        .iter()
        .map(|host| {
            let fresh = snapshots.contains_key(host);
            let known = displayed.get(host);
            HostView {
                host: host.clone(),
                label: labels.get(host).cloned().unwrap_or_default(),
                reachable: fresh,
                stale: !fresh && known.is_some(),
                error: errors.get(host).cloned(),
                observed_at: known.map(|snapshot| snapshot.generated_at.clone()),
                runs: known
                    .map(|snapshot| snapshot.runs.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();
    StateView {
        schema_version: SCHEMA_VERSION,
        generated_at: utc_now(),
        interval_seconds: interval.as_secs_f64(),
        hosts,
    }
}

fn handle_connection(mut stream: TcpStream, broadcast: &Broadcast) {
    let Some(request) = read_request(&stream) else {
        return;
    };
    let path = request.path.split('?').next().unwrap_or("/").to_owned();
    if request.method == "POST" {
        if path == "/api/label" {
            handle_label(&mut stream, broadcast, &request.body);
        } else {
            let _ = write_simple(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"not found",
            );
        }
        return;
    }
    if request.method != "GET" && request.method != "HEAD" {
        let _ = write_simple(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            b"method not allowed",
        );
        return;
    }
    match path.as_str() {
        "/" | "/index.html" => {
            let _ = write_simple(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                INDEX_HTML.as_bytes(),
            );
        }
        "/api/state" => {
            let (_, payload) = broadcast.current();
            let _ = write_simple(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                payload.as_bytes(),
            );
        }
        "/api/stream" => stream_events(stream, broadcast),
        _ => {
            let _ = write_simple(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"not found",
            );
        }
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?).take(MAX_REQUEST_LINE);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();

    let mut length = 0usize;
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {
                if let Some((name, value)) = header.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            Err(_) => break,
        }
    }

    let mut body = Vec::new();
    if length > 0 {
        // `take` already bounds this; the cap keeps a bogus header from allocating.
        reader
            .take(length.min(MAX_BODY_BYTES) as u64)
            .read_to_end(&mut body)
            .ok()?;
    }
    Some(Request { method, path, body })
}

fn handle_label(stream: &mut TcpStream, broadcast: &Broadcast, body: &[u8]) {
    let request = serde_json::from_slice::<LabelRequest>(body);
    let Ok(request) = request else {
        let _ = write_simple(
            stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            b"expected {\"host\":\"...\",\"label\":\"...\"}",
        );
        return;
    };
    let label = sanitize(&request.label);
    // Reject unknown hosts so a stray request cannot append to the config.
    match broadcast.apply_label(&request.host, &label) {
        Ok(false) => {
            let _ = write_simple(
                stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"unknown host",
            );
            return;
        }
        Ok(true) => {}
        Err(error) => {
            eprintln!("llm-watch: could not publish label: {error}");
        }
    }
    if let Err(error) = set_label(&labels_path(), &request.host, &label) {
        eprintln!("llm-watch: could not save label: {error}");
        let _ = write_simple(
            stream,
            "500 Internal Server Error",
            "text/plain; charset=utf-8",
            b"could not save label",
        );
        return;
    }
    let body = serde_json::json!({ "host": request.host, "label": label }).to_string();
    let _ = write_simple(
        stream,
        "200 OK",
        "application/json; charset=utf-8",
        body.as_bytes(),
    );
}

fn stream_events(mut stream: TcpStream, broadcast: &Broadcast) {
    let headers = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Cache-Control: no-cache, no-store\r\n",
        "Connection: close\r\n",
        "X-Accel-Buffering: no\r\n",
        "\r\n",
    );
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }
    let _ = stream.set_nodelay(true);
    let (mut version, payload) = broadcast.current();
    if send_event(&mut stream, &payload).is_err() {
        return;
    }
    loop {
        match broadcast.wait_after(version, HEARTBEAT) {
            Some((next, payload)) => {
                version = next;
                if send_event(&mut stream, &payload).is_err() {
                    return;
                }
            }
            // Keeps the connection warm and surfaces a dropped client as a write error.
            None => {
                if stream.write_all(b": ping\n\n").is_err() || stream.flush().is_err() {
                    return;
                }
            }
        }
    }
}

fn send_event(stream: &mut TcpStream, payload: &str) -> std::io::Result<()> {
    // A payload is single-line JSON, so one data field is enough.
    stream.write_all(b"data: ")?;
    stream.write_all(payload.as_bytes())?;
    stream.write_all(b"\n\n")?;
    stream.flush()
}

fn write_simple(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(error) = Command::new(opener).arg(url).status() {
        if error.kind() != ErrorKind::NotFound {
            eprintln!("llm-watch: could not open browser: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(host: &str, state: &str) -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            host: host.to_owned(),
            generated_at: "2026-08-07T00:00:00Z".to_owned(),
            runs: vec![RunRecord {
                state: state.to_owned(),
                ..RunRecord::default()
            }],
            events: vec![],
        }
    }

    #[test]
    fn unreachable_hosts_keep_last_known_runs_and_are_marked_stale() {
        let hosts = vec!["dev-a".to_owned(), "dev-b".to_owned()];
        let fresh = BTreeMap::from([("dev-a".to_owned(), snapshot("dev-a", "ready"))]);
        let displayed = BTreeMap::from([
            ("dev-a".to_owned(), snapshot("dev-a", "ready")),
            ("dev-b".to_owned(), snapshot("dev-b", "working")),
        ]);
        let errors = BTreeMap::from([("dev-b".to_owned(), "SSH timed out".to_owned())]);

        let labels = BTreeMap::from([("dev-b".to_owned(), "payments train".to_owned())]);
        let state = build_state(
            &hosts,
            &fresh,
            &displayed,
            &errors,
            &labels,
            Duration::from_secs_f64(5.0),
        );

        assert!(state.hosts[0].reachable && !state.hosts[0].stale);
        assert!(!state.hosts[1].reachable && state.hosts[1].stale);
        assert_eq!(state.hosts[1].error.as_deref(), Some("SSH timed out"));
        assert_eq!(state.hosts[1].runs[0].state, "working");
        // An unlabelled host reports an empty label, not a missing key.
        assert_eq!(state.hosts[0].label, "");
        assert_eq!(state.hosts[1].label, "payments train");
    }

    #[test]
    fn never_seen_hosts_report_no_runs() {
        let hosts = vec!["dev-new".to_owned()];
        let state = build_state(
            &hosts,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::from([("dev-new".to_owned(), "connection refused".to_owned())]),
            &BTreeMap::new(),
            Duration::from_secs_f64(5.0),
        );
        assert!(!state.hosts[0].reachable);
        assert!(!state.hosts[0].stale);
        assert!(state.hosts[0].runs.is_empty());
        assert!(state.hosts[0].observed_at.is_none());
    }

    fn view(hosts: &[&str]) -> StateView {
        StateView {
            schema_version: SCHEMA_VERSION,
            generated_at: "2026-08-07T00:00:00Z".to_owned(),
            interval_seconds: 5.0,
            hosts: hosts
                .iter()
                .map(|host| HostView {
                    host: (*host).to_owned(),
                    label: String::new(),
                    reachable: true,
                    stale: false,
                    error: None,
                    observed_at: None,
                    runs: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn broadcast_wakes_waiters_with_the_new_payload() {
        let broadcast = Arc::new(Broadcast::new(view(&["dev-a"])).unwrap());
        let (version, payload) = broadcast.current();
        assert!(payload.contains("dev-a"));
        assert!(broadcast
            .wait_after(version, Duration::from_millis(20))
            .is_none());

        let writer = Arc::clone(&broadcast);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            writer.publish(view(&["dev-b"])).unwrap();
        });
        let (next, payload) = broadcast
            .wait_after(version, Duration::from_secs(5))
            .expect("publish should wake the waiter");
        handle.join().unwrap();
        assert_eq!(next, version + 1);
        assert!(payload.contains("dev-b"));
    }

    #[test]
    fn a_label_edit_republishes_immediately_and_rejects_unknown_hosts() {
        let broadcast = Broadcast::new(view(&["dev-a", "dev-b"])).unwrap();
        let (version, _) = broadcast.current();

        assert!(broadcast.apply_label("dev-b", "render tree").unwrap());
        let (next, payload) = broadcast.current();
        assert_eq!(next, version + 1, "viewers are woken without a new poll");
        assert!(payload.contains("render tree"));

        // An unknown host must not mutate or bump anything.
        assert!(!broadcast.apply_label("dev-nope", "x").unwrap());
        assert_eq!(broadcast.current().0, next);
    }
}
