use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_EVENT_LIMIT: usize = 200;
pub const DEFAULT_RETENTION_DAYS: u64 = 14;
pub const TASK_LIMIT: usize = 120;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Event {
    pub schema_version: u32,
    pub event_id: String,
    pub provider: String,
    pub event: String,
    pub session_id: String,
    pub run_id: String,
    pub host: String,
    pub cwd: String,
    pub project: String,
    pub tmux: String,
    pub task: String,
    pub state: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunRecord {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub tmux: String,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub updated_at: String,
}

impl From<Event> for RunRecord {
    fn from(event: Event) -> Self {
        Self {
            schema_version: event.schema_version,
            provider: event.provider,
            session_id: event.session_id,
            run_id: event.run_id,
            host: event.host,
            cwd: event.cwd,
            project: event.project,
            tmux: event.tmux,
            task: event.task,
            state: event.state,
            updated_at: event.updated_at,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    pub schema_version: u32,
    pub host: String,
    pub generated_at: String,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DashboardCache {
    #[serde(default)]
    pub initialized_hosts: Vec<String>,
    #[serde(default)]
    pub seen_events: Vec<String>,
    #[serde(default)]
    pub last_snapshots: BTreeMap<String, Snapshot>,
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}
