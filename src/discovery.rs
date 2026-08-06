use crate::storage::{config_dir, expand_home, home_dir};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn discover_hosts(
    hosts_file: Option<&Path>,
    ssh_config: Option<&Path>,
    ignore_file: Option<&Path>,
    include_coder: bool,
) -> Vec<String> {
    let mut hosts = BTreeSet::new();
    let hosts_path = hosts_file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir().join("hosts"));
    hosts.extend(read_list_file(&hosts_path));

    let ssh_path = ssh_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home_dir().join(".ssh/config"));
    let mut seen = BTreeSet::new();
    parse_ssh_file(&ssh_path, &mut seen, &mut hosts);
    if include_coder {
        hosts.extend(discover_coder_hosts());
    }

    // The ignore list is applied last so it can suppress any discovery source.
    // Hosts named explicitly on the command line never reach here.
    let ignore_path = ignore_file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir().join("ignore"));
    let patterns = read_list_file(&ignore_path);
    hosts
        .into_iter()
        .filter(|host| !is_ignored(host, &patterns))
        .collect()
}

/// Reads a config list: one entry per line, `#` begins a comment.
fn read_list_file(path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let entry = line.split('#').next().unwrap_or_default().trim();
            (!entry.is_empty()).then(|| entry.to_owned())
        })
        .collect()
}

/// An entry matches a host literally, or as a glob when it contains `*` or `?`.
pub fn is_ignored(host: &str, patterns: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    patterns
        .iter()
        .any(|pattern| wildcard_matches(&pattern.to_ascii_lowercase(), &host))
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
        // Every input is pinned to the fixture so the developer's own config cannot leak in.
        fs::write(root.join("ignore"), "# nothing ignored\n").unwrap();
        let hosts = discover_hosts(
            Some(&root.join("hosts")),
            Some(&root.join("config")),
            Some(&root.join("ignore")),
            false,
        );
        assert_eq!(hosts, vec!["coder.dev-eu", "dev-api", "dev-ops", "dev-web"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ignored_hosts_are_dropped_from_every_discovery_source() {
        let root = std::env::temp_dir().join(format!("llm-watch-ignore-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        // coder.dev-eu arrives from the hosts file, dev-web and dev-api from ssh config.
        fs::write(root.join("hosts"), "coder.dev-eu\ndev-ops\n").unwrap();
        fs::write(root.join("config"), "Host dev-api dev-web\n").unwrap();
        fs::write(
            root.join("ignore"),
            "# retired\ncoder.dev-eu\ndev-w*\n\n  dev-api  # trailing comment\n",
        )
        .unwrap();
        let hosts = discover_hosts(
            Some(&root.join("hosts")),
            Some(&root.join("config")),
            Some(&root.join("ignore")),
            false,
        );
        assert_eq!(hosts, vec!["dev-ops"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ignore_matching_is_literal_unless_a_glob_is_used() {
        let patterns = vec!["coder.dev-eu".to_owned(), "lab-*".to_owned()];
        assert!(is_ignored("coder.dev-eu", &patterns));
        assert!(is_ignored("CODER.DEV-EU", &patterns));
        assert!(is_ignored("lab-01", &patterns));
        // A literal entry must not behave like a prefix.
        assert!(!is_ignored("coder.dev-eu-2", &patterns));
        assert!(!is_ignored("coder.dev2", &patterns));
        assert!(!is_ignored("dev-lab-01", &patterns));
        assert!(!is_ignored("anything", &[]));
    }
}
