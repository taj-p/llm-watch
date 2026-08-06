//! Raise the local WezTerm window on the tab that belongs to a devbox.
//!
//! The dashboard runs on the same laptop as the terminal, so a card can jump you
//! straight to the tab instead of leaving you to hunt for it. Tabs are matched by
//! their explicit title: `wezterm.lua`'s `gui-startup` handler calls
//! `tab:set_title(host)`, so a tab's `tab_title` is exactly the SSH alias the
//! dashboard already keys hosts by.

use crate::dashboard::{command_exists, output_with_timeout};
use std::process::Command;
use std::time::Duration;

/// What happened, so the page can say something specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Activated,
    /// WezTerm is up but nothing is titled with that alias.
    NoTab,
    /// No GUI to talk to: not running, or its socket is gone.
    NotRunning,
    NotInstalled,
}

impl Focus {
    /// The wire value the page switches on.
    pub fn status(self) -> &'static str {
        match self {
            Focus::Activated => "activated",
            Focus::NoTab => "no_tab",
            Focus::NotRunning => "not_running",
            Focus::NotInstalled => "not_installed",
        }
    }
}

/// Picks the tab whose explicit title matches the alias. Titles come from
/// `wezterm.lua`; ids are stable but indices drift as tabs open and close, so the
/// id is the only safe handle to activate by.
pub fn tab_id_for_host(list_json: &str, host: &str) -> Option<i64> {
    if host.is_empty() {
        return None;
    }
    let entries = serde_json::from_str::<serde_json::Value>(list_json).ok()?;
    // One entry per pane, so several rows can share a tab; the first is enough.
    entries
        .as_array()?
        .iter()
        .find(|entry| entry.get("tab_title").and_then(|title| title.as_str()) == Some(host))
        .and_then(|entry| entry.get("tab_id"))
        .and_then(serde_json::Value::as_i64)
}

pub fn focus_host(host: &str, timeout: Duration) -> Focus {
    // A separate probe because `output_with_timeout` flattens a spawn failure to a
    // string, losing the "no such binary" distinction we want to report.
    if !command_exists("wezterm") {
        return Focus::NotInstalled;
    }
    let listed = match cli(&["cli", "list", "--format", "json"], timeout) {
        Some(output) => output,
        None => return Focus::NotRunning,
    };
    let Some(tab) = tab_id_for_host(&listed, host) else {
        return Focus::NoTab;
    };
    let id = tab.to_string();
    if cli(&["cli", "activate-tab", "--tab-id", &id], timeout).is_none() {
        return Focus::NotRunning;
    }
    raise_app();
    Focus::Activated
}

/// Runs a `wezterm` subcommand, returning its stdout, or `None` when there is no
/// live GUI to talk to.
fn cli(arguments: &[&str], timeout: Duration) -> Option<String> {
    let mut command = Command::new("wezterm");
    command.args(arguments);
    // A stale `WEZTERM_UNIX_SOCKET` — inherited from whichever shell started the
    // server — makes the CLI fail *and* spawn a stray `wezterm-mux-server`.
    // Removing it lets the CLI resolve the live GUI through its stable symlink.
    command.env_remove("WEZTERM_UNIX_SOCKET");
    match output_with_timeout(&mut command, timeout) {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        // A dead or absent GUI looks like a non-zero exit; a wedged mux looks like
        // a timeout. Neither is worth distinguishing on the page.
        _ => None,
    }
}

/// `activate-tab` switches the tab but leaves WezTerm behind the browser.
fn raise_app() {
    if !cfg!(target_os = "macos") {
        return;
    }
    // The bundle id survives a renamed or relocated app, unlike `open -a WezTerm`.
    let _ = Command::new("open")
        .args(["-b", "com.github.wez.wezterm"])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from real `wezterm cli list --format json` output: the local `bl`
    /// tab, three devbox tabs, a gap where `coder.dev-eu`'s tab was closed, and a
    /// hand-opened tab with no title.
    const LIST: &str = r#"[
      {"window_id":0,"tab_id":0,"pane_id":0,"title":"Tajs-MacBook-Pro","tab_title":"bl"},
      {"window_id":0,"tab_id":1,"pane_id":1,"title":"dev2","tab_title":"coder.dev2"},
      {"window_id":0,"tab_id":2,"pane_id":2,"title":"dev3","tab_title":"coder.dev3"},
      {"window_id":0,"tab_id":2,"pane_id":4,"title":"dev3 pane 2","tab_title":"coder.dev3"},
      {"window_id":0,"tab_id":3,"pane_id":3,"title":"lsr-dash","tab_title":"coder.lsr-dash"},
      {"window_id":0,"tab_id":5,"pane_id":6,"title":"zsh","tab_title":""}
    ]"#;

    #[test]
    fn a_devbox_tab_is_found_by_its_title() {
        assert_eq!(tab_id_for_host(LIST, "coder.dev3"), Some(2));
        assert_eq!(tab_id_for_host(LIST, "coder.lsr-dash"), Some(3));
        // Ids are sparse once a tab is closed, so position must not be assumed.
        assert_eq!(tab_id_for_host(LIST, "coder.dev2"), Some(1));
    }

    #[test]
    fn a_host_with_no_tab_is_not_matched() {
        // coder.dev-eu is configured in wezterm.lua but its tab was closed.
        assert_eq!(tab_id_for_host(LIST, "coder.dev-eu"), None);
    }

    #[test]
    fn untitled_and_local_tabs_are_never_matched() {
        // The hand-opened tab has an empty title; an empty host must not claim it.
        assert_eq!(tab_id_for_host(LIST, ""), None);
        // The local tab is titled, but not with a host alias.
        assert_eq!(tab_id_for_host(LIST, "Tajs-MacBook-Pro"), None);
    }

    #[test]
    fn malformed_output_is_tolerated() {
        for text in ["", "not json", "{}", "[]", "[1,2,3]", "[{\"tab_id\":1}]"] {
            assert_eq!(tab_id_for_host(text, "coder.dev3"), None);
        }
    }
}
