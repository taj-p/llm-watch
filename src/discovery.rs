use crate::storage::{config_dir, expand_home, home_dir};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn discover_hosts(
    hosts_file: Option<&Path>,
    ssh_config: Option<&Path>,
    include_coder: bool,
) -> Vec<String> {
    let mut hosts = BTreeSet::new();
    let hosts_path = hosts_file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir().join("hosts"));
    if let Ok(content) = fs::read_to_string(hosts_path) {
        for line in content.lines() {
            let host = line.split('#').next().unwrap_or_default().trim();
            if !host.is_empty() {
                hosts.insert(host.to_owned());
            }
        }
    }

    let ssh_path = ssh_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home_dir().join(".ssh/config"));
    let mut seen = BTreeSet::new();
    parse_ssh_file(&ssh_path, &mut seen, &mut hosts);
    if include_coder {
        hosts.extend(discover_coder_hosts());
    }
    hosts.into_iter().collect()
}

fn parse_ssh_file(path: &Path, seen: &mut BTreeSet<PathBuf>, hosts: &mut BTreeSet<String>) {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| expand_home(path.to_path_buf()));
    if !seen.insert(resolved.clone()) {
        return;
    }
    let Ok(content) = fs::read_to_string(&resolved) else {
        return;
    };
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = split_directive(line) else {
            continue;
        };
        if key.eq_ignore_ascii_case("include") {
            for pattern in rest.split_whitespace() {
                let expanded = expand_home(PathBuf::from(pattern));
                let absolute = if expanded.is_absolute() {
                    expanded
                } else {
                    resolved.parent().unwrap_or(Path::new(".")).join(expanded)
                };
                for included in expand_pattern(&absolute) {
                    parse_ssh_file(&included, seen, hosts);
                }
            }
        } else if key.eq_ignore_ascii_case("host") {
            for host in rest.split_whitespace() {
                if host.starts_with('!') || host.chars().any(|c| matches!(c, '*' | '?' | '[' | ']'))
                {
                    continue;
                }
                if host.starts_with("dev") {
                    hosts.insert(host.to_owned());
                }
            }
        }
    }
}

fn split_directive(line: &str) -> Option<(&str, &str)> {
    let index = line.find(char::is_whitespace)?;
    Some((&line[..index], line[index..].trim()))
}

fn expand_pattern(pattern: &Path) -> Vec<PathBuf> {
    let text = pattern.to_string_lossy();
    if !text.contains(['*', '?']) {
        return vec![pattern.to_path_buf()];
    }
    let parent = pattern.parent().unwrap_or(Path::new("."));
    let file_pattern = pattern
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matches = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            wildcard_matches(file_pattern, name).then_some(entry.path())
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star, mut checkpoint) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            checkpoint = v;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            checkpoint += 1;
            v = checkpoint;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

pub fn coder_hosts_from_json(value: &Value) -> Vec<String> {
    let mut hosts = BTreeSet::new();
    let Some(workspaces) = value.as_array() else {
        return Vec::new();
    };
    for workspace in workspaces {
        let name = workspace
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = workspace
            .get("latest_build")
            .and_then(|build| build.get("status"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.starts_with("dev") && status.eq_ignore_ascii_case("running") {
            hosts.insert(format!("coder.{name}"));
        }
    }
    hosts.into_iter().collect()
}

fn discover_coder_hosts() -> Vec<String> {
    let Ok(output) = Command::new("coder")
        .args(["list", "--output", "json"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    serde_json::from_slice(&output.stdout)
        .ok()
        .map(|value| coder_hosts_from_json(&value))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn running_dev_workspaces_become_coder_aliases() {
        let workspaces = json!([
            {"name": "dev2", "latest_build": {"status": "running"}},
            {"name": "dev-eu", "latest_build": {"status": "running"}},
            {"name": "dev-old", "latest_build": {"status": "stopped"}},
            {"name": "experiment", "latest_build": {"status": "running"}}
        ]);
        assert_eq!(
            coder_hosts_from_json(&workspaces),
            vec!["coder.dev-eu".to_owned(), "coder.dev2".to_owned()]
        );
    }

    #[test]
    fn explicit_and_included_hosts_are_discovered() {
        let root = std::env::temp_dir().join(format!("llm-watch-ssh-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("conf.d")).unwrap();
        fs::write(root.join("conf.d/work"), "Host dev-web\n").unwrap();
        fs::write(
            root.join("config"),
            "Include conf.d/*\nHost dev-api coder.dev-eu\nHost dev*\n",
        )
        .unwrap();
        fs::write(root.join("hosts"), "coder.dev-eu\ndev-ops\n").unwrap();
        let hosts = discover_hosts(Some(&root.join("hosts")), Some(&root.join("config")), false);
        assert_eq!(hosts, vec!["coder.dev-eu", "dev-api", "dev-ops", "dev-web"]);
        let _ = fs::remove_dir_all(root);
    }
}
