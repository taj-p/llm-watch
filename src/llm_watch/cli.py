from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any, Mapping, Sequence

from . import __version__
from .core import (
    DEFAULT_EVENT_LIMIT,
    NOTIFY_STATES,
    config_dir,
    discover_hosts,
    normalize_event,
    read_json,
    render_table,
    snapshot,
    state_dir,
    utc_now,
    write_event,
)


def payload_from_input(payload_arg: str | None) -> Mapping[str, Any]:
    raw = payload_arg
    if raw is None and not sys.stdin.isatty():
        raw = sys.stdin.read()
    if not raw:
        return {}
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("hook payload must be a JSON object")
    return value


def hook_error(message: str) -> None:
    try:
        root = state_dir()
        root.mkdir(parents=True, exist_ok=True)
        with (root / "hook-errors.log").open("a", encoding="utf-8") as handle:
            handle.write(f"{utc_now()} {message}\n")
    except OSError:
        pass


def command_hook(args: argparse.Namespace) -> int:
    try:
        payload = payload_from_input(args.payload)
        event = normalize_event(args.provider, payload, explicit_event=args.event)
        if event is not None:
            write_event(event)
    except Exception as error:  # Hooks must never interrupt the agent loop.
        hook_error(f"{args.provider}: {error}")
    return 0


def command_snapshot(args: argparse.Namespace) -> int:
    print(json.dumps(snapshot(event_limit=args.events), sort_keys=True))
    return 0


def command_hosts(args: argparse.Namespace) -> int:
    hosts = discover_hosts(
        hosts_file=Path(args.hosts_file).expanduser() if args.hosts_file else None,
        ssh_config=Path(args.ssh_config).expanduser() if args.ssh_config else None,
        include_coder=not args.no_coder,
    )
    for host in hosts:
        print(host)
    return 0 if hosts else 1


def fetch_snapshot(
    host: str, timeout: float, event_limit: int
) -> tuple[str, dict[str, Any] | None, str | None]:
    command = f"~/.local/bin/llm-watch snapshot --events {event_limit}"
    try:
        result = subprocess.run(
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                f"ConnectTimeout={max(1, int(timeout))}",
                host,
                command,
            ],
            capture_output=True,
            text=True,
            timeout=timeout + 2,
        )
    except subprocess.TimeoutExpired:
        return host, None, "SSH timed out"
    except OSError as error:
        return host, None, str(error)
    if result.returncode != 0:
        message = result.stderr.strip().splitlines()
        return host, None, message[-1] if message else f"SSH exited {result.returncode}"
    value: Any = None
    for line in reversed(result.stdout.splitlines()):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            break
    if not isinstance(value, dict):
        return host, None, "invalid snapshot response"
    return host, value, None


def fetch_all(
    hosts: Sequence[str], timeout: float, event_limit: int
) -> tuple[dict[str, Any], dict[str, str]]:
    snapshots: dict[str, Any] = {}
    errors: dict[str, str] = {}
    if not hosts:
        return snapshots, errors
    with ThreadPoolExecutor(max_workers=min(16, len(hosts))) as executor:
        futures = [
            executor.submit(fetch_snapshot, host, timeout, event_limit) for host in hosts
        ]
        for future in as_completed(futures):
            host, value, error = future.result()
            if value is not None:
                snapshots[host] = value
            elif error:
                errors[host] = error
    return snapshots, errors


def cache_path() -> Path:
    return state_dir() / "dashboard-cache.json"


def load_cache() -> dict[str, Any]:
    return read_json(cache_path()) or {
        "initialized_hosts": [],
        "seen_events": [],
        "last_snapshots": {},
    }


def save_cache(cache: Mapping[str, Any]) -> None:
    from .core import atomic_json

    atomic_json(cache_path(), cache)


def notify(title: str, message: str) -> None:
    configured = os.environ.get("LLM_WATCH_NOTIFY_COMMAND")
    try:
        if configured:
            subprocess.run(
                shlex.split(configured) + [title, message], check=False, timeout=5
            )
        elif shutil.which("terminal-notifier"):
            subprocess.run(
                [
                    "terminal-notifier",
                    "-title",
                    title,
                    "-message",
                    message,
                    "-group",
                    "llm-watch",
                ],
                check=False,
                timeout=5,
            )
        elif sys.platform == "darwin" and shutil.which("osascript"):
            script = (
                "on run argv\n"
                "display notification (item 2 of argv) with title (item 1 of argv)\n"
                "end run"
            )
            subprocess.run(
                ["osascript", "-e", script, "--", title, message],
                check=False,
                timeout=5,
            )
        elif shutil.which("notify-send"):
            subprocess.run(["notify-send", title, message], check=False, timeout=5)
    except (OSError, subprocess.SubprocessError):
        pass


def process_notifications(snapshots: Mapping[str, Mapping[str, Any]], enabled: bool) -> None:
    cache = load_cache()
    initialized = set(str(value) for value in cache.get("initialized_hosts", []))
    seen_order = [str(value) for value in cache.get("seen_events", [])]
    seen = set(seen_order)
    current_by_host: dict[str, dict[str, str]] = {}
    for host, data in snapshots.items():
        current_by_host[host] = {
            str(run.get("run_id")): str(run.get("state"))
            for run in data.get("runs", [])
            if isinstance(run, Mapping)
        }
        events = data.get("events", [])
        if host not in initialized:
            for event in events:
                if isinstance(event, Mapping) and event.get("event_id"):
                    event_id = str(event["event_id"])
                    if event_id not in seen:
                        seen.add(event_id)
                        seen_order.append(event_id)
            initialized.add(host)
            continue
        for event in events:
            if not isinstance(event, Mapping):
                continue
            event_id = str(event.get("event_id") or "")
            if not event_id or event_id in seen:
                continue
            seen.add(event_id)
            seen_order.append(event_id)
            state = str(event.get("state") or "")
            run_id = str(event.get("run_id") or "")
            if state not in NOTIFY_STATES or current_by_host[host].get(run_id) != state:
                continue
            if enabled:
                tool = str(event.get("provider") or "LLM").capitalize()
                task = str(event.get("task") or event.get("project") or "session")
                notify(f"LLM Watch: {state.upper()}", f"{host} · {tool} · {task}")
    cache["initialized_hosts"] = sorted(initialized)
    cache["seen_events"] = seen_order[-5000:]
    last_snapshots = cache.get("last_snapshots", {})
    if not isinstance(last_snapshots, dict):
        last_snapshots = {}
    last_snapshots.update(snapshots)
    cache["last_snapshots"] = last_snapshots
    save_cache(cache)


def with_last_known(
    snapshots: Mapping[str, Mapping[str, Any]], hosts: Sequence[str]
) -> dict[str, Mapping[str, Any]]:
    cached = load_cache().get("last_snapshots", {})
    combined: dict[str, Mapping[str, Any]] = {}
    if isinstance(cached, Mapping):
        for host in hosts:
            value = cached.get(host)
            if isinstance(value, Mapping):
                combined[host] = value
    combined.update(snapshots)
    return combined


def resolve_hosts(args: argparse.Namespace) -> list[str]:
    if args.hosts:
        return sorted(dict.fromkeys(args.hosts))
    return discover_hosts(
        hosts_file=Path(args.hosts_file).expanduser() if args.hosts_file else None,
        ssh_config=Path(args.ssh_config).expanduser() if args.ssh_config else None,
        include_coder=not args.no_coder,
    )


def command_dashboard(args: argparse.Namespace) -> int:
    hosts = resolve_hosts(args)
    if not hosts:
        print(
            f"No dev* SSH aliases found. Add hosts to {config_dir() / 'hosts'}.",
            file=sys.stderr,
        )
        return 1
    while True:
        snapshots, errors = fetch_all(hosts, args.timeout, args.events)
        process_notifications(snapshots, not args.no_notify)
        displayed = with_last_known(snapshots, hosts)
        table = render_table(displayed, errors, show_stopped=args.all)
        if args.once:
            print(table)
            return 0 if snapshots else 1
        if sys.stdout.isatty():
            print("\033[2J\033[H", end="")
        print(table)
        print(f"\nUpdated {utc_now()} · Ctrl+C to stop", flush=True)
        try:
            time.sleep(args.interval)
        except KeyboardInterrupt:
            return 130


def command_list(args: argparse.Namespace) -> int:
    local = snapshot(event_limit=0)
    print(render_table({local["host"]: local}, show_stopped=args.all))
    return 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        prog="llm-watch", description="Watch Codex and Claude sessions across devboxes"
    )
    result.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    commands = result.add_subparsers(dest="command", required=True)

    hook = commands.add_parser("hook", help="record a Codex or Claude lifecycle event")
    hook.add_argument("provider", choices=("codex", "claude"))
    hook.add_argument("payload", nargs="?", help="JSON payload; defaults to stdin")
    hook.add_argument("--event", help="event name when it is absent from the payload")
    hook.set_defaults(func=command_hook)

    snap = commands.add_parser("snapshot", help="print this machine's state as JSON")
    snap.add_argument("--events", type=int, default=DEFAULT_EVENT_LIMIT)
    snap.set_defaults(func=command_snapshot)

    local = commands.add_parser("list", help="show sessions recorded on this machine")
    local.add_argument("--all", action="store_true", help="include stopped sessions")
    local.set_defaults(func=command_list)

    hosts = commands.add_parser("hosts", help="list configured and discovered devboxes")
    hosts.add_argument("--hosts-file")
    hosts.add_argument("--ssh-config")
    hosts.add_argument("--no-coder", action="store_true")
    hosts.set_defaults(func=command_hosts)

    dashboard = commands.add_parser(
        "dashboard", help="poll devboxes and show a combined dashboard"
    )
    dashboard.add_argument(
        "hosts", nargs="*", help="SSH aliases; defaults to discovered dev* aliases"
    )
    dashboard.add_argument("--hosts-file")
    dashboard.add_argument("--ssh-config")
    dashboard.add_argument("--no-coder", action="store_true")
    dashboard.add_argument("--interval", type=float, default=5.0)
    dashboard.add_argument("--timeout", type=float, default=3.0)
    dashboard.add_argument("--events", type=int, default=DEFAULT_EVENT_LIMIT)
    dashboard.add_argument("--once", action="store_true")
    dashboard.add_argument("--all", action="store_true", help="include stopped sessions")
    dashboard.add_argument("--no-notify", action="store_true")
    dashboard.set_defaults(func=command_dashboard)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
