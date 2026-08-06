use crate::hooks::compact_text;
use crate::storage::{atomic_write, config_dir, AppResult};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{fs, io};

pub const LABEL_LIMIT: usize = 60;

const HEADER: &str = "\
# Work-stream labels for the llm-watch dashboard.
# One `host = label` per line; `#` starts a comment.
# Edited in place by the web dashboard.
";

pub fn labels_path() -> PathBuf {
    config_dir().join("labels")
}

/// Reads `host = label` lines. Anything unparseable is skipped rather than
/// failing the poll that asked for it.
pub fn read_labels(path: &Path) -> BTreeMap<String, String> {
    let Ok(content) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut labels = BTreeMap::new();
    for line in content.lines() {
        let entry = line.split('#').next().unwrap_or_default().trim();
        let Some((host, label)) = entry.split_once('=') else {
            continue;
        };
        let (host, label) = (host.trim(), label.trim());
        if !host.is_empty() && !label.is_empty() {
            labels.insert(host.to_owned(), label.to_owned());
        }
    }
    labels
}

/// Strips what the line format cannot represent, then bounds the length.
pub fn sanitize(label: &str) -> String {
    let cleaned = label.replace(['#', '\n', '\r'], " ");
    compact_text(&cleaned, LABEL_LIMIT)
}

/// Sets or, when `label` is empty, clears one host's label. Other hosts keep
/// theirs, so two dashboards editing different hosts cannot erase each other.
pub fn set_label(path: &Path, host: &str, label: &str) -> AppResult<String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("host must not be empty".into());
    }
    let label = sanitize(label);
    let mut labels = read_labels(path);
    if label.is_empty() {
        labels.remove(host);
    } else {
        labels.insert(host.to_owned(), label.clone());
    }
    atomic_write(path, |writer| {
        writer.write_all(HEADER.as_bytes())?;
        for (host, label) in &labels {
            writeln!(writer, "{host} = {label}")?;
        }
        Ok(())
    })
    .map_err(|error| io::Error::other(format!("could not write {}: {error}", path.display())))?;
    Ok(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::temporary_test_dir;

    #[test]
    fn labels_round_trip_and_ignore_junk() {
        let root = temporary_test_dir("labels");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("labels");
        fs::write(
            &path,
            "# heading\ncoder.dev2 = payments train\nbroken line\n  = orphan\ncoder.dev3=  \n",
        )
        .unwrap();

        let labels = read_labels(&path);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels["coder.dev2"], "payments train");

        set_label(&path, "coder.dev3", "  render   tree  ").unwrap();
        let labels = read_labels(&path);
        assert_eq!(labels["coder.dev2"], "payments train", "other hosts kept");
        assert_eq!(labels["coder.dev3"], "render tree", "whitespace collapsed");

        // An empty label clears just that host.
        set_label(&path, "coder.dev2", "").unwrap();
        let labels = read_labels(&path);
        assert!(!labels.contains_key("coder.dev2"));
        assert_eq!(labels["coder.dev3"], "render tree");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_label_cannot_break_the_line_format() {
        let root = temporary_test_dir("labels-escape");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("labels");
        // `#` would comment out the rest, and a newline would forge a second entry.
        set_label(&path, "coder.dev2", "urgent # coder.dev3 = hijacked\nx").unwrap();
        let labels = read_labels(&path);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels["coder.dev2"], "urgent coder.dev3 = hijacked x");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn long_labels_are_truncated() {
        assert_eq!(sanitize(&"a".repeat(200)).chars().count(), LABEL_LIMIT);
        assert!(set_label(Path::new("/nonexistent/x/labels"), "", "x").is_err());
    }
}
