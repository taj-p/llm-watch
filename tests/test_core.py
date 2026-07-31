import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from llm_watch import core


class NormalizeEventTests(unittest.TestCase):
    def test_codex_notify_becomes_ready(self):
        payload = {
            "type": "agent-turn-complete",
            "thread-id": "thread-1",
            "cwd": "/workspace/payments",
            "input-messages": ["Add retries to payment requests"],
        }
        with mock.patch.object(core, "short_host", return_value="dev-api"):
            event = core.normalize_event("codex", payload, environ={})

        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event["state"], "ready")
        self.assertEqual(event["run_id"], "codex-thread-1")
        self.assertEqual(event["task"], "Add retries to payment requests")
        self.assertEqual(event["project"], "payments")

    def test_claude_permission_notification_needs_approval(self):
        payload = {
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "session_id": "claude-1",
            "cwd": "/workspace/web",
            "message": "Claude needs permission",
        }
        event = core.normalize_event("claude", payload, environ={})
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event["state"], "approval")

    def test_irrelevant_notification_is_ignored(self):
        event = core.normalize_event(
            "claude",
            {
                "hook_event_name": "Notification",
                "notification_type": "auth_success",
            },
            environ={},
        )
        self.assertIsNone(event)

    def test_compaction_does_not_make_a_running_session_idle(self):
        event = core.normalize_event(
            "codex",
            {
                "hook_event_name": "SessionStart",
                "source": "compact",
                "session_id": "session-1",
            },
            environ={},
        )
        self.assertIsNone(event)

    def test_environment_task_wins(self):
        event = core.normalize_event(
            "claude",
            {
                "hook_event_name": "UserPromptSubmit",
                "session_id": "session-1",
                "prompt": "Automatic prompt label",
            },
            environ={"LLM_WATCH_TASK": "Human label"},
        )
        assert event is not None
        self.assertEqual(event["task"], "Human label")


class StateTests(unittest.TestCase):
    def test_write_event_preserves_task_and_snapshot_contains_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            running = {
                "schema_version": 1,
                "event_id": "event-1",
                "provider": "codex",
                "event": "UserPromptSubmit",
                "session_id": "one",
                "run_id": "codex-one",
                "host": "dev-one",
                "cwd": "/workspace/app",
                "project": "app",
                "tmux": "app:1.0",
                "task": "Implement the feature",
                "state": "running",
                "updated_at": "2026-08-01T00:00:00Z",
            }
            ready = dict(running)
            ready.update(
                {
                    "event_id": "event-2",
                    "event": "agent-turn-complete",
                    "task": "",
                    "state": "ready",
                    "updated_at": "2026-08-01T00:01:00Z",
                }
            )

            core.write_event(running, root)
            core.write_event(ready, root)
            result = core.snapshot(root, event_limit=10)

            self.assertEqual(len(result["runs"]), 1)
            self.assertEqual(result["runs"][0]["task"], "Implement the feature")
            self.assertEqual(result["runs"][0]["state"], "ready")
            self.assertEqual(
                [event["event_id"] for event in result["events"]],
                ["event-1", "event-2"],
            )

    def test_permission_event_does_not_replace_task(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base = {
                "schema_version": 1,
                "provider": "claude",
                "session_id": "one",
                "run_id": "claude-one",
                "host": "dev-one",
                "cwd": "/workspace/app",
                "project": "app",
                "tmux": "app:1.0",
                "updated_at": "2026-08-01T00:00:00Z",
            }
            core.write_event(
                dict(
                    base,
                    event_id="event-1",
                    event="UserPromptSubmit",
                    task="Implement the feature",
                    state="running",
                ),
                root,
            )
            core.write_event(
                dict(
                    base,
                    event_id="event-2",
                    event="Notification",
                    task="",
                    state="approval",
                ),
                root,
            )
            result = core.snapshot(root)
            self.assertEqual(result["runs"][0]["task"], "Implement the feature")

    def test_atomic_json_is_valid(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested/state.json"
            core.atomic_json(path, {"hello": "world"})
            self.assertEqual(json.loads(path.read_text()), {"hello": "world"})


class HostDiscoveryTests(unittest.TestCase):
    def test_explicit_and_literal_dev_hosts_are_combined(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            includes = root / "conf.d"
            includes.mkdir()
            (includes / "work").write_text("Host dev-web\n  HostName web.example\n")
            ssh_config = root / "config"
            ssh_config.write_text(
                "Include conf.d/*\n"
                "Host dev-api coder.dev-eu\n"
                "  User taj\n"
                "Host dev*\n"
                "  ForwardAgent yes\n"
            )
            hosts_file = root / "hosts"
            hosts_file.write_text("coder.dev-eu\n# ignored\ndev-ops # comment\n")

            hosts = core.discover_hosts(
                hosts_file=hosts_file,
                ssh_config=ssh_config,
                include_coder=False,
            )

            self.assertEqual(hosts, ["coder.dev-eu", "dev-api", "dev-ops", "dev-web"])

    def test_running_dev_coder_workspaces_become_ssh_aliases(self):
        workspaces = [
            {"name": "dev2", "latest_build": {"status": "running"}},
            {"name": "dev-eu", "latest_build": {"status": "running"}},
            {"name": "dev-old", "latest_build": {"status": "stopped"}},
            {"name": "experiment", "latest_build": {"status": "running"}},
        ]
        self.assertEqual(
            core.coder_hosts_from_json(workspaces),
            ["coder.dev-eu", "coder.dev2"],
        )


class RenderTests(unittest.TestCase):
    def test_table_uses_ssh_alias_and_hides_stopped(self):
        snapshots = {
            "dev-api": {
                "runs": [
                    {
                        "provider": "codex",
                        "project": "payments",
                        "task": "Add retries",
                        "state": "ready",
                        "tmux": "pay:1.0",
                        "updated_at": "2026-08-01T00:00:00Z",
                    },
                    {"state": "stopped"},
                ]
            }
        }
        table = core.render_table(snapshots)
        self.assertIn("dev-api", table)
        self.assertIn("Add retries", table)
        self.assertNotIn("STOPPED", table)


if __name__ == "__main__":
    unittest.main()
