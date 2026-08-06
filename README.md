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

- Rust and Cargo when installing from source
- SSH access from the dashboard machine to each devbox
- Codex CLI and/or Claude Code on the machines being watched
- Optional: tmux, for pane names in the dashboard
- Optional: `terminal-notifier` on macOS or `notify-send` on Linux

`llm-watch` is a native Rust binary and has no runtime language dependency.

## Install

Clone this repository on the laptop and every devbox, then run:

```sh
./install.sh
```

The installer builds an optimized, locked release and creates
`~/.local/bin/llm-watch` as a symlink to `target/release/llm-watch`. Verify it
with:

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

To stop watching a host without deleting it from `~/.ssh/config` or shutting the
Coder workspace down, list it in:

```text
~/.config/llm-watch/ignore
```

The format matches the `hosts` file: one entry per line, `#` starts a comment.
Entries match a host literally, or as a glob when they contain `*` or `?`:

```text
coder.dev-eu     # retired, keep the SSH alias
lab-*
```

The ignore list is applied after every discovery source, so it also suppresses
entries coming from the `hosts` file. Hosts named directly on the command line
bypass it, so `llm-watch dashboard coder.dev-eu` still works. Pass
`--ignore-file` to use a different file.

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

## Run the web dashboard

For a browser view of the same data, with no page reloads:

```sh
llm-watch serve --open
```

This polls the same discovered devboxes over SSH and serves
`http://127.0.0.1:8787/`. The browser holds a server-sent events connection, so
every poll is pushed to the page as it completes and the tab reconnects on its
own if the server restarts.

The headline answers one question per devbox: is it working or not. The summary
counts working boxes, then the approvals and errors that need you. `READY` is
not counted in either — a finished session is simply a box that is not working.

Below that is a card per devbox, marked `WORKING` or `IDLE`, listing its Codex
and Claude sessions with tool, project, tmux pane, task, age, and the agent's
last message. Working sessions sort first, then approvals and errors. Stopped
sessions are hidden behind a toggle. An unreachable devbox keeps its last known
sessions on screen, marked offline with the SSH error and the time it was last
seen — the same last-known behaviour as the terminal dashboard.

### Work-stream labels

Each card carries a free-text label for the work stream it is on. Click the
label on a card to edit it; press Enter to save or Escape to cancel. Labels are
stored in `~/.config/llm-watch/labels` and shared by every open tab:

```text
coder.dev3 = lightspeed render tree
coder.lsr-dash = depot upload PR train
```

The file can also be edited by hand — it is re-read on every poll, so changes
appear without restarting the server. Clearing a label removes its line.

### Last assistant message

Each session shows the agent's most recent message, so a devbox going `READY`
carries the context of where it got to. Codex supplies this in its `notify`
payload; for Claude it is read from the tail of the session transcript. The
message is captured when the hook fires and is kept through the approval and
session events that follow, until the next completed turn replaces it.

This is recorded **on the devbox**, so each devbox needs a build of `llm-watch`
new enough to capture it. Until one is updated, its sessions simply show no
message; nothing else is affected. Messages also only appear for turns that
complete after the update — existing sessions stay blank until their next turn.

```sh
llm-watch serve --port 9000 --interval 10
llm-watch serve dev-api dev-web
```

Notifications are off here, since the page is already visible; pass `--notify`
to also get desktop notifications. Running `serve` and `dashboard` at once is
supported — they share the notification baseline, so an event notifies once.

The server binds to loopback only. `--bind 0.0.0.0` exposes the page, and
everything on it, to your whole network; it has no authentication.

Two endpoints are available if you want to build on the data:

| Method | Path | Behaviour |
|--------|------|-----------|
| `GET` | `/api/state` | The latest poll as a JSON object |
| `GET` | `/api/stream` | The same object pushed on every poll as `text/event-stream` |
| `POST` | `/api/label` | `{"host": "...", "label": "..."}` sets a label; an empty label clears it |

`POST /api/label` rejects a host that is not being watched, so it can only edit
aliases already on the dashboard.

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
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

The state files are versioned JSON. Per-session snapshots are written using an
atomic rename, while recent events are retained as individual immutable files.
Both are pruned after 14 days.
