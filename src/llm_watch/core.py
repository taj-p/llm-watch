from __future__ import annotations

import contextlib
import datetime as dt
import fcntl
import glob
import json
import os
import re
import shutil
import socket
import subprocess
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Iterator, Mapping, Sequence


SCHEMA_VERSION = 1
DEFAULT_EVENT_LIMIT = 200
DEFAULT_RETENTION_DAYS = 14
TASK_LIMIT = 120
NOTIFY_STATES = {"ready", "approval", "error"}


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds").replace(
        "+00:00", "Z"
    )


def parse_time(value: str | None) -> dt.datetime | None:
    if not value:
        return None
    try:
        return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def state_dir(environ: Mapping[str, str] | None = None) -> Path:
    env = os.environ if environ is None else environ
    override = env.get("LLM_WATCH_STATE_DIR")
    if override:
        return Path(override).expanduser()
    xdg_state = env.get("XDG_STATE_HOME")
    if xdg_state:
        return Path(xdg_state).expanduser() / "llm-watch"
    return Path(env.get("HOME", str(Path.home()))).expanduser() / ".local/state/llm-watch"


def config_dir(environ: Mapping[str, str] | None = None) -> Path:
    env = os.environ if environ is None else environ
    override = env.get("LLM_WATCH_CONFIG_DIR")
    if override:
        return Path(override).expanduser()
    xdg_config = env.get("XDG_CONFIG_HOME")
    if xdg_config:
        return Path(xdg_config).expanduser() / "llm-watch"
    return Path(env.get("HOME", str(Path.home()))).expanduser() / ".config/llm-watch"


def short_host() -> str:
    return socket.gethostname().split(".", 1)[0]


def compact_text(value: Any, limit: int = TASK_LIMIT) -> str:
    if value is None:
        return ""
    if isinstance(value, (list, tuple)):
        value = " ".join(str(item) for item in value if item is not None)
    elif isinstance(value, dict):
        value = value.get("text") or value.get("content") or ""
    text = re.sub(r"\s+", " ", str(value)).strip()
    if len(text) <= limit:
        return text
    return text[: limit - 1].rstrip() + "…"


def safe_name(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "-", value).strip("-.")
    return cleaned[:160] or "unknown"


def tmux_target(environ: Mapping[str, str] | None = None) -> str:
    env = os.environ if environ is None else environ
    pane = env.get("TMUX_PANE")
    if not pane:
        return ""
    try:
        result = subprocess.run(
            [
                "tmux",
                "display-message",
                "-p",
                "-t",
                pane,
                "#{session_name}:#{window_index}.#{pane_index}",
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=1,
            env=dict(env),
        )
    except (OSError, subprocess.SubprocessError):
        return pane
    return result.stdout.strip() or pane


def payload_event(payload: Mapping[str, Any], explicit: str | None = None) -> str:
    return str(
        explicit
        or payload.get("hook_event_name")
        or payload.get("hookEventName")
        or payload.get("type")
        or "unknown"
    )


def normalize_state(provider: str, event: str, payload: Mapping[str, Any]) -> str | None:
    normalized = event.replace("_", "-").lower()
    if normalized in {"userpromptsubmit", "user-prompt-submit"}:
        return "running"
    if normalized in {"agent-turn-complete", "stop"}:
        return "ready"
    if normalized in {"stopfailure", "stop-failure"}:
        return "error"
    if normalized in {"sessionend", "session-end"}:
        return "stopped"
    if normalized in {"sessionstart", "session-start"}:
        if str(payload.get("source") or "").lower() == "compact":
            return None
        return "idle"
    if normalized in {"permissionrequest", "permission-request"}:
        return "approval"
    if normalized == "notification":
        notification_type = str(
            payload.get("notification_type") or payload.get("notification-type") or ""
        ).lower()
        if notification_type == "permission_prompt":
            return "approval"
        if notification_type == "idle_prompt":
            return "ready"
        return None
    return None


def session_id(provider: str, payload: Mapping[str, Any], environ: Mapping[str, str]) -> str:
    for key in ("session_id", "session-id", "thread_id", "thread-id"):
        value = payload.get(key)
        if value:
            return str(value)
    fallback = "|".join(
        (
            provider,
            str(payload.get("cwd") or os.getcwd()),
            environ.get("TMUX_PANE", ""),
        )
    )
    return str(uuid.uuid5(uuid.NAMESPACE_URL, fallback))


def task_from_payload(payload: Mapping[str, Any], keys: Sequence[str]) -> str:
    for key in keys:
        value = payload.get(key)
        text = compact_text(value)
        if text:
            return text
    return ""


def normalize_event(
    provider: str,
    payload: Mapping[str, Any],
    *,
    explicit_event: str | None = None,
    environ: Mapping[str, str] | None = None,
) -> dict[str, Any] | None:
    env = os.environ if environ is None else environ
    provider = provider.lower()
    event = payload_event(payload, explicit_event)
    state = normalize_state(provider, event, payload)
    if state is None:
        return None
    sid = session_id(provider, payload, env)
    now = utc_now()
    cwd = str(payload.get("cwd") or os.getcwd())
    task = compact_text(env.get("LLM_WATCH_TASK"))
    if not task and state == "running":
        task = task_from_payload(
            payload, ("prompt", "input", "input_messages", "input-messages")
        )
    elif not task and event.replace("_", "-").lower() == "agent-turn-complete":
        task = task_from_payload(payload, ("input_messages", "input-messages"))
    return {
        "schema_version": SCHEMA_VERSION,
        "event_id": str(uuid.uuid4()),
        "provider": provider,
        "event": event,
        "session_id": sid,
        "run_id": f"{provider}-{sid}",
        "host": short_host(),
        "cwd": cwd,
        "project": Path(cwd).name or cwd,
        "tmux": tmux_target(env),
        "task": task,
        "state": state,
        "updated_at": now,
    }


@contextlib.contextmanager
def locked(path: Path) -> Iterator[None]:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+", encoding="utf-8") as handle:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def read_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def atomic_json(path: Path, value: Mapping[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(value, handle, sort_keys=True, separators=(",", ":"))
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_name, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(temp_name)
        raise


def write_event(event: Mapping[str, Any], root: Path | None = None) -> dict[str, Any]:
    base = state_dir() if root is None else root
    run_path = base / "runs" / f"{safe_name(str(event['run_id']))}.json"
    with locked(base / ".lock"):
        previous = read_json(run_path) or {}
        merged = dict(previous)
        merged.update(event)
        if not event.get("task") and previous.get("task"):
            merged["task"] = previous["task"]
        if not event.get("tmux") and previous.get("tmux"):
            merged["tmux"] = previous["tmux"]
        merged.pop("event_id", None)
        merged.pop("event", None)
        atomic_json(run_path, merged)

        stamp = time.time_ns()
        event_path = base / "events" / f"{stamp:020d}-{event['event_id']}.json"
        atomic_json(event_path, event)
    prune(base)
    return merged


def prune(root: Path, retention_days: int = DEFAULT_RETENTION_DAYS) -> None:
    cutoff = time.time() - retention_days * 86400
    for folder in (root / "events", root / "runs"):
        if not folder.exists():
            continue
        for path in folder.glob("*.json"):
            try:
                if path.stat().st_mtime < cutoff:
                    path.unlink()
            except OSError:
                continue


def snapshot(
    root: Path | None = None, event_limit: int = DEFAULT_EVENT_LIMIT
) -> dict[str, Any]:
    base = state_dir() if root is None else root
    runs: list[dict[str, Any]] = []
    events: list[dict[str, Any]] = []
    with locked(base / ".lock"):
        for path in sorted((base / "runs").glob("*.json")):
            value = read_json(path)
            if value:
                runs.append(value)
        event_paths = sorted((base / "events").glob("*.json"), reverse=True)
        for path in reversed(event_paths[: max(0, event_limit)]):
            value = read_json(path)
            if value:
                events.append(value)
    runs.sort(key=lambda item: str(item.get("updated_at", "")), reverse=True)
    return {
        "schema_version": SCHEMA_VERSION,
        "host": short_host(),
        "generated_at": utc_now(),
        "runs": runs,
        "events": events,
    }


def _parse_ssh_file(path: Path, seen: set[Path]) -> list[str]:
    try:
        resolved = path.expanduser().resolve()
    except OSError:
        resolved = path.expanduser()
    if resolved in seen:
        return []
    seen.add(resolved)
    try:
        lines = resolved.read_text(encoding="utf-8").splitlines()
    except OSError:
        return []

    hosts: list[str] = []
    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        key, _, rest = line.partition(" ")
        if not rest:
            key, _, rest = line.partition("\t")
        if key.lower() == "include":
            for pattern in rest.split():
                expanded = os.path.expanduser(pattern)
                if not os.path.isabs(expanded):
                    expanded = str(resolved.parent / expanded)
                for included in sorted(glob.glob(expanded)):
                    hosts.extend(_parse_ssh_file(Path(included), seen))
        elif key.lower() == "host":
            for host in rest.split():
                if host.startswith("!") or any(char in host for char in "*?[]"):
                    continue
                if host.startswith("dev"):
                    hosts.append(host)
    return hosts


def discover_hosts(
    *,
    hosts_file: Path | None = None,
    ssh_config: Path | None = None,
    environ: Mapping[str, str] | None = None,
    include_coder: bool = True,
) -> list[str]:
    env = os.environ if environ is None else environ
    configured = hosts_file or config_dir(env) / "hosts"
    hosts: list[str] = []
    try:
        for raw in configured.read_text(encoding="utf-8").splitlines():
            host = raw.split("#", 1)[0].strip()
            if host:
                hosts.append(host)
    except OSError:
        pass
    ssh_path = ssh_config or Path(env.get("HOME", str(Path.home()))) / ".ssh/config"
    hosts.extend(_parse_ssh_file(ssh_path, set()))
    if include_coder:
        hosts.extend(discover_coder_hosts())
    return sorted(dict.fromkeys(hosts))


def coder_hosts_from_json(value: Any) -> list[str]:
    if not isinstance(value, list):
        return []
    hosts: list[str] = []
    for workspace in value:
        if not isinstance(workspace, Mapping):
            continue
        name = str(workspace.get("name") or "")
        latest_build = workspace.get("latest_build")
        status = ""
        if isinstance(latest_build, Mapping):
            status = str(latest_build.get("status") or "").lower()
        if name.startswith("dev") and status == "running":
            hosts.append(f"coder.{name}")
    return sorted(dict.fromkeys(hosts))


def discover_coder_hosts() -> list[str]:
    if not shutil.which("coder"):
        return []
    try:
        result = subprocess.run(
            ["coder", "list", "--output", "json"],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if result.returncode != 0:
        return []
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError:
        return []
    return coder_hosts_from_json(value)


def age(value: str | None, now: dt.datetime | None = None) -> str:
    timestamp = parse_time(value)
    if timestamp is None:
        return "?"
    current = now or dt.datetime.now(dt.timezone.utc)
    seconds = max(0, int((current - timestamp).total_seconds()))
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m"
    if seconds < 86400:
        return f"{seconds // 3600}h"
    return f"{seconds // 86400}d"


def render_table(
    snapshots: Mapping[str, Mapping[str, Any]],
    errors: Mapping[str, str] | None = None,
    *,
    show_stopped: bool = False,
) -> str:
    rows: list[list[str]] = []
    for alias, data in sorted(snapshots.items()):
        runs = data.get("runs", [])
        if not isinstance(runs, Sequence):
            continue
        for run in runs:
            if not isinstance(run, Mapping):
                continue
            state = str(run.get("state", "unknown"))
            if state == "stopped" and not show_stopped:
                continue
            rows.append(
                [
                    alias,
                    str(run.get("tmux") or "-"),
                    str(run.get("provider") or "-"),
                    str(run.get("project") or "-"),
                    compact_text(run.get("task") or "-", 54),
                    state.upper(),
                    age(str(run.get("updated_at") or "")),
                ]
            )
    for alias, message in sorted((errors or {}).items()):
        rows.append([alias, "-", "-", "-", compact_text(message, 54), "UNREACHABLE", "-"])
    headers = ["HOST", "TMUX", "TOOL", "PROJECT", "TASK", "STATE", "AGE"]
    if not rows:
        return "No active LLM sessions found."
    widths = [len(value) for value in headers]
    for row in rows:
        widths = [max(width, len(value)) for width, value in zip(widths, row)]
    lines = ["  ".join(value.ljust(width) for value, width in zip(headers, widths))]
    lines.append("  ".join("-" * width for width in widths))
    for row in rows:
        lines.append("  ".join(value.ljust(width) for value, width in zip(row, widths)))
    return "\n".join(lines)
