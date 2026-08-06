mod dashboard;
mod discovery;
mod hooks;
mod labels;
mod model;
mod server;
mod storage;

use clap::{Parser, Subcommand, ValueEnum};
use dashboard::{fetch_all, process_notifications, render_table, with_last_known};
use discovery::discover_hosts;
use hooks::{compact_text, normalize_event, parse_payload, tmux_target};
use model::{Link, DEFAULT_EVENT_LIMIT, SCHEMA_VERSION, TASK_LIMIT};
use serde_json::Map;
use server::{serve, ServeOptions};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;
use storage::{
    clear_link, config_dir, snapshot, state_dir, utc_now, write_event, write_link, AppResult,
};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "llm-watch",
    version,
    about = "Watch Codex and Claude sessions across devboxes"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Record a Codex or Claude lifecycle event.
    Hook {
        #[arg(value_enum)]
        provider: Provider,
        /// JSON payload; defaults to stdin.
        payload: Option<String>,
        /// Event name when it is absent from the payload.
        #[arg(long)]
        event: Option<String>,
    },
    /// Publish or withdraw a web UI this machine is serving.
    Link {
        #[command(subcommand)]
        action: LinkAction,
    },
    /// Print this machine's state as JSON.
    Snapshot {
        #[arg(long, default_value_t = DEFAULT_EVENT_LIMIT)]
        events: usize,
    },
    /// Show sessions recorded on this machine.
    List {
        #[arg(long)]
        all: bool,
    },
    /// List configured and discovered devboxes.
    Hosts {
        #[arg(long)]
        hosts_file: Option<PathBuf>,
        #[arg(long)]
        ssh_config: Option<PathBuf>,
        /// File of hosts to skip; defaults to the `ignore` file in the config directory.
        #[arg(long)]
        ignore_file: Option<PathBuf>,
        #[arg(long)]
        no_coder: bool,
    },
    /// Poll devboxes and show a combined dashboard.
    Dashboard {
        /// SSH aliases; defaults to discovered dev* Coder workspaces.
        hosts: Vec<String>,
        #[arg(long)]
        hosts_file: Option<PathBuf>,
        #[arg(long)]
        ssh_config: Option<PathBuf>,
        /// File of hosts to skip; defaults to the `ignore` file in the config directory.
        #[arg(long)]
        ignore_file: Option<PathBuf>,
        #[arg(long)]
        no_coder: bool,
        #[arg(long, default_value_t = 5.0)]
        interval: f64,
        #[arg(long, default_value_t = 3.0)]
        timeout: f64,
        #[arg(long, default_value_t = DEFAULT_EVENT_LIMIT)]
        events: usize,
        #[arg(long)]
        once: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        no_notify: bool,
    },
    /// Poll devboxes and serve a live web dashboard.
    Serve {
        /// SSH aliases; defaults to discovered dev* Coder workspaces.
        hosts: Vec<String>,
        #[arg(long)]
        hosts_file: Option<PathBuf>,
        #[arg(long)]
        ssh_config: Option<PathBuf>,
        /// File of hosts to skip; defaults to the `ignore` file in the config directory.
        #[arg(long)]
        ignore_file: Option<PathBuf>,
        #[arg(long)]
        no_coder: bool,
        /// Address to bind. Loopback by default; anything else exposes the page to your network.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        #[arg(long, default_value_t = 5.0)]
        interval: f64,
        #[arg(long, default_value_t = 3.0)]
        timeout: f64,
        #[arg(long, default_value_t = DEFAULT_EVENT_LIMIT)]
        events: usize,
        /// Also send desktop notifications, as `dashboard` does.
        #[arg(long)]
        notify: bool,
        /// Open the dashboard in the default browser once it is listening.
        #[arg(long)]
        open: bool,
    },
}

#[derive(Debug, Subcommand)]
enum LinkAction {
    /// Publish a URL, or refresh one already published. Re-run within the TTL
    /// to keep it alive; a link that stops being refreshed disappears.
    Set {
        /// The URL to open. Must be http or https.
        #[arg(long)]
        url: String,
        /// Stable identifier; defaults to one derived from kind, cwd, and tmux pane.
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "link")]
        kind: String,
        /// Shown on the dashboard; defaults to the directory name.
        #[arg(long)]
        title: Option<String>,
    },
    /// Withdraw a published URL.
    Clear {
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "link")]
        kind: String,
    },
    /// Print the live links on this machine.
    List,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Provider {
    Codex,
    Claude,
}

impl Provider {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("llm-watch: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> AppResult<u8> {
    match cli.command {
        Commands::Hook {
            provider,
            payload,
            event,
        } => {
            command_hook(provider, payload, event.as_deref());
            Ok(0)
        }
        Commands::Link { action } => command_link(action),
        Commands::Snapshot { events } => {
            println!("{}", serde_json::to_string(&snapshot(events)?)?);
            Ok(0)
        }
        Commands::List { all } => {
            let local = snapshot(0)?;
            let table = render_table(
                &BTreeMap::from([(local.host.clone(), local)]),
                &BTreeMap::new(),
                all,
            );
            println!("{table}");
            Ok(0)
        }
        Commands::Hosts {
            hosts_file,
            ssh_config,
            ignore_file,
            no_coder,
        } => {
            let hosts = discover_hosts(
                hosts_file.as_deref(),
                ssh_config.as_deref(),
                ignore_file.as_deref(),
                !no_coder,
            );
            for host in &hosts {
                println!("{host}");
            }
            Ok(if hosts.is_empty() { 1 } else { 0 })
        }
        Commands::Dashboard {
            hosts,
            hosts_file,
            ssh_config,
            ignore_file,
            no_coder,
            interval,
            timeout,
            events,
            once,
            all,
            no_notify,
        } => command_dashboard(DashboardOptions {
            hosts,
            hosts_file,
            ssh_config,
            ignore_file,
            no_coder,
            interval,
            timeout,
            events,
            once,
            all,
            no_notify,
        }),
        Commands::Serve {
            hosts,
            hosts_file,
            ssh_config,
            ignore_file,
            no_coder,
            bind,
            port,
            interval,
            timeout,
            events,
            notify,
            open,
        } => {
            let Some(hosts) = resolve_hosts(
                hosts,
                hosts_file.as_deref(),
                ssh_config.as_deref(),
                ignore_file.as_deref(),
                no_coder,
            ) else {
                return Ok(1);
            };
            serve(ServeOptions {
                hosts,
                bind,
                port,
                interval: Duration::from_secs_f64(interval.max(0.1)),
                timeout: Duration::from_secs_f64(timeout.max(0.1)),
                events,
                notify,
                open,
            })
        }
    }
}

/// Explicit aliases win; otherwise fall back to discovery. `None` means nothing to watch.
fn resolve_hosts(
    explicit: Vec<String>,
    hosts_file: Option<&Path>,
    ssh_config: Option<&Path>,
    ignore_file: Option<&Path>,
    no_coder: bool,
) -> Option<Vec<String>> {
    let mut hosts = if explicit.is_empty() {
        discover_hosts(hosts_file, ssh_config, ignore_file, !no_coder)
    } else {
        explicit
    };
    hosts.sort();
    hosts.dedup();
    if hosts.is_empty() {
        eprintln!(
            "No dev* SSH aliases found. Add hosts to {}.",
            config_dir().join("hosts").display()
        );
        return None;
    }
    Some(hosts)
}

/// The dashboard puts these URLs in an iframe `src` and an anchor `href`, so
/// only http and https are accepted — never `javascript:` or `data:`.
fn checked_url(url: &str) -> AppResult<String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(format!("url must start with http:// or https://, got {url:?}").into());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("url must not contain whitespace or control characters".into());
    }
    Ok(url.to_owned())
}

/// Without an explicit id, a link belongs to its kind, directory, and tmux
/// pane, so re-running the same command in place replaces its own entry.
fn derived_link_id(kind: &str, cwd: &str) -> String {
    let seed = format!("{kind}|{cwd}|{}", env::var("TMUX_PANE").unwrap_or_default());
    format!(
        "{kind}-{}",
        Uuid::new_v5(&Uuid::NAMESPACE_URL, seed.as_bytes())
    )
}

fn command_link(action: LinkAction) -> AppResult<u8> {
    let cwd = env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_owned());
    match action {
        LinkAction::Set {
            url,
            id,
            kind,
            title,
        } => {
            let url = checked_url(&url)?;
            let project = Path::new(&cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&cwd)
                .to_owned();
            let link = Link {
                schema_version: SCHEMA_VERSION,
                id: id.unwrap_or_else(|| derived_link_id(&kind, &cwd)),
                kind: kind.clone(),
                url,
                title: compact_text(&title.unwrap_or_else(|| project.clone()), TASK_LIMIT),
                cwd: cwd.clone(),
                project,
                tmux: tmux_target(),
                updated_at: utc_now(),
            };
            write_link(&link)?;
            Ok(0)
        }
        LinkAction::Clear { id, kind } => {
            clear_link(&id.unwrap_or_else(|| derived_link_id(&kind, &cwd)))?;
            Ok(0)
        }
        LinkAction::List => {
            for link in snapshot(0)?.links {
                println!("{}\t{}\t{}", link.kind, link.title, link.url);
            }
            Ok(0)
        }
    }
}

fn command_hook(
    provider: Provider,
    payload_argument: Option<String>,
    explicit_event: Option<&str>,
) {
    let result = (|| -> AppResult<()> {
        let raw = match payload_argument {
            Some(payload) => payload,
            None if !io::stdin().is_terminal() => {
                let mut payload = String::new();
                io::stdin().read_to_string(&mut payload)?;
                payload
            }
            None => String::new(),
        };
        let payload = if raw.trim().is_empty() {
            Map::new()
        } else {
            parse_payload(&raw)?
        };
        if let Some(event) = normalize_event(provider.as_str(), &payload, explicit_event) {
            write_event(&event)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        log_hook_error(provider.as_str(), &error.to_string());
    }
}

fn log_hook_error(provider: &str, message: &str) {
    let root = state_dir();
    if fs::create_dir_all(&root).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("hook-errors.log"))
    {
        let _ = writeln!(file, "{} {provider}: {message}", utc_now());
    }
}

struct DashboardOptions {
    hosts: Vec<String>,
    hosts_file: Option<PathBuf>,
    ssh_config: Option<PathBuf>,
    ignore_file: Option<PathBuf>,
    no_coder: bool,
    interval: f64,
    timeout: f64,
    events: usize,
    once: bool,
    all: bool,
    no_notify: bool,
}

fn command_dashboard(options: DashboardOptions) -> AppResult<u8> {
    let Some(hosts) = resolve_hosts(
        options.hosts,
        options.hosts_file.as_deref(),
        options.ssh_config.as_deref(),
        options.ignore_file.as_deref(),
        options.no_coder,
    ) else {
        return Ok(1);
    };

    let timeout = Duration::from_secs_f64(options.timeout.max(0.1));
    let interval = Duration::from_secs_f64(options.interval.max(0.1));
    loop {
        let (snapshots, errors) = fetch_all(&hosts, timeout, options.events);
        process_notifications(&snapshots, !options.no_notify)?;
        let displayed = with_last_known(&snapshots, &hosts);
        let table = render_table(&displayed, &errors, options.all);
        if options.once {
            println!("{table}");
            return Ok(if snapshots.is_empty() { 1 } else { 0 });
        }
        if io::stdout().is_terminal() {
            print!("\x1b[2J\x1b[H");
        }
        println!("{table}\n\n{}", dashboard::status_line());
        io::stdout().flush()?;
        thread::sleep(interval);
    }
}
