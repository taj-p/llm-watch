# llm-watch

`llm-watch` shows what Codex and Claude are doing across SSH-accessible
development boxes and sends a desktop notification when a session is ready,
needs approval, or fails.

It is deliberately pull-based. Agent hooks only write local state on each
devbox. The laptop polls the boxes it can already access over SSH, so no
devbox-to-devbox networking, tunnel, webhook, or central service is required.

```text
Codex / Claude hooks              Laptop
        |                           |
        v                           | SSH polling
~/.local/state/llm-watch <----------+
                                    |
                                    +--> terminal dashboard
                                    +--> desktop notification
```

## Requirements

- Python 3.9 or later
- SSH access from the dashboard machine to each devbox
- Codex CLI and/or Claude Code on the machines being watched
- Optional: tmux, for pane names in the dashboard
- Optional: `terminal-notifier` on macOS or `notify-send` on Linux

There are no Python package dependencies.

## Install

Clone this repository on the laptop and every devbox, then run:

```sh
./install.sh
```

The installer creates `~/.local/bin/llm-watch` as a symlink to the checkout.
Verify it with:

```sh
llm-watch --version
```

## Configure agent hooks

### Codex

Add the following to the user-level `~/.codex/config.toml`. The `notify`
setting records completed turns. Lifecycle hooks record starts, permission
requests, and session closure.

```toml
notify = ["llm-watch", "hook", "codex"]

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "llm-watch hook codex"
timeout = 2

[[hooks.PermissionRequest]]
[[hooks.PermissionRequest.hooks]]
type = "command"
command = "llm-watch hook codex"
timeout = 2

[[hooks.SessionStart]]
matcher = "^(startup|resume|clear)$"
[[hooks.SessionStart.hooks]]
type = "command"
command = "llm-watch hook codex"
timeout = 2

[[hooks.SessionEnd]]
[[hooks.SessionEnd.hooks]]
type = "command"
command = "llm-watch hook codex"
timeout = 1
```

Codex requires review before newly configured command hooks can run. Start
Codex, open `/hooks`, and trust the `llm-watch` hooks. `notify` is a user-level
setting and must not be placed in a project `.codex/config.toml`.

### Claude Code

Merge the following keys into `~/.claude/settings.json`. Do not replace other
hooks already present in that file.

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "llm-watch hook claude",
            "timeout": 2
          }
        ]
      }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt",
        "hooks": [
          {
            "type": "command",
            "command": "llm-watch hook claude",
            "timeout": 2
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "llm-watch hook claude",
            "timeout": 2
          }
        ]
      }
    ],
    "StopFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "llm-watch hook claude",
            "timeout": 2
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "startup|resume|clear",
        "hooks": [
          {
            "type": "command",
            "command": "llm-watch hook claude",
            "timeout": 2
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "llm-watch hook claude",
            "timeout": 2
          }
        ]
      }
    ]
  }
}
```

Run `/hooks` in Claude Code to confirm that the hooks are loaded.

Hook failures are recorded in
`~/.local/state/llm-watch/hook-errors.log` and always return success so a
monitoring problem cannot interrupt the agent.

## Discover devboxes

On the laptop, `llm-watch` runs `coder list --output json` once at startup,
selects running Coder workspaces whose names begin with `dev`, and polls their
`coder.<workspace>` SSH aliases. For example, the workspace `dev-eu` is reached
as `coder.dev-eu`.

It also discovers literal SSH aliases beginning with `dev` from
`~/.ssh/config` and its `Include` files. Wildcard clauses such as `Host dev*`
configure SSH but cannot enumerate actual machines. Pass `--no-coder` to
`hosts` or `dashboard` to skip Coder discovery.

Add aliases that are dynamic or do not start with `dev` to:

```text
~/.config/llm-watch/hosts
```

One host is listed per line:

```text
dev-api
dev-web
coder.dev-eu
```

Check discovery with:

```sh
llm-watch hosts
```

SSH multiplexing makes frequent polling inexpensive. A useful laptop config is:

```sshconfig
Host coder.dev* dev*
    BatchMode yes
    ConnectTimeout 3
    ControlMaster auto
    ControlPath ~/.ssh/control-%C
    ControlPersist 10m
```

## Run the dashboard

Start a continuously refreshing dashboard on the laptop:

```sh
llm-watch dashboard
```

Or print one snapshot:

```sh
llm-watch dashboard --once
llm-watch dashboard --once dev-api dev-web
```

Stopped sessions are hidden by default. Use `--all` to include them and
`--no-notify` to disable desktop notifications.

The first successful observation of a host establishes a notification
baseline. Later `READY`, `APPROVAL`, and `ERROR` events trigger notifications.
Recent events remain on the devbox, so events that happen while the laptop is
asleep can be observed after reconnecting. An old approval is not shown if the
session has since moved to another state.

macOS notifications use `terminal-notifier` when installed and otherwise fall
back to `osascript`. Linux uses `notify-send`. To use another notifier, set a
command that accepts the title and message as its final two arguments:

```sh
export LLM_WATCH_NOTIFY_COMMAND="$HOME/bin/my-notifier"
```

## Local commands

On an individual devbox:

```sh
llm-watch list
llm-watch list --all
llm-watch snapshot
```

State is stored under `~/.local/state/llm-watch`, or under
`$XDG_STATE_HOME/llm-watch` when set. Set `LLM_WATCH_TASK` before launching an
agent to override the automatically derived prompt label.

## Development

```sh
PYTHONPATH=src python3 -m unittest discover -s tests -v
```

The state files are versioned JSON. Per-session snapshots are written using an
atomic rename, while recent events are retained as individual immutable files.
Both are pruned after 14 days.
