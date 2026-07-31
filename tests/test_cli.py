import argparse
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from llm_watch import cli


class HookCommandTests(unittest.TestCase):
    def test_hook_failure_is_non_blocking(self):
        args = argparse.Namespace(provider="codex", payload="not json", event=None)
        with mock.patch.object(cli, "hook_error") as error:
            status = cli.command_hook(args)
        self.assertEqual(status, 0)
        error.assert_called_once()


class DashboardNotificationTests(unittest.TestCase):
    def snapshot(self, state="ready"):
        event = {
            "event_id": "event-1",
            "run_id": "codex-one",
            "provider": "codex",
            "project": "payments",
            "task": "Add retries",
            "state": state,
        }
        return {"runs": [event], "events": [event]}

    def test_first_observation_sets_baseline_without_notifying(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(cli, "state_dir", return_value=Path(directory)):
                with mock.patch.object(cli, "notify") as notify:
                    cli.process_notifications({"dev-api": self.snapshot()}, True)
                notify.assert_not_called()

    def test_new_current_event_notifies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(cli, "state_dir", return_value=root):
                cli.process_notifications({"dev-api": {"runs": [], "events": []}}, True)
                with mock.patch.object(cli, "notify") as notify:
                    cli.process_notifications({"dev-api": self.snapshot()}, True)
                notify.assert_called_once_with(
                    "LLM Watch: READY", "dev-api · Codex · Add retries"
                )

    def test_resolved_approval_does_not_notify(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            approval = self.snapshot("approval")
            approval["runs"][0] = dict(approval["runs"][0], state="ready")
            with mock.patch.object(cli, "state_dir", return_value=root):
                cli.process_notifications({"dev-api": {"runs": [], "events": []}}, True)
                with mock.patch.object(cli, "notify") as notify:
                    cli.process_notifications({"dev-api": approval}, True)
                notify.assert_not_called()

    def test_last_known_snapshot_is_retained(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(cli, "state_dir", return_value=root):
                cli.process_notifications({"dev-api": self.snapshot()}, False)
                combined = cli.with_last_known({}, ["dev-api"])
            self.assertEqual(combined["dev-api"]["runs"][0]["state"], "ready")


class FetchTests(unittest.TestCase):
    def test_fetch_snapshot_uses_batch_mode(self):
        completed = mock.Mock(returncode=0, stdout=json.dumps({"runs": []}), stderr="")
        with mock.patch("subprocess.run", return_value=completed) as run:
            host, value, error = cli.fetch_snapshot("dev-api", 3, 20)
        self.assertEqual(host, "dev-api")
        self.assertEqual(value, {"runs": []})
        self.assertIsNone(error)
        command = run.call_args.args[0]
        self.assertIn("BatchMode=yes", command)
        self.assertIn("dev-api", command)

    def test_fetch_snapshot_ignores_remote_banner(self):
        completed = mock.Mock(
            returncode=0,
            stdout="Welcome to the devbox\n" + json.dumps({"runs": []}) + "\n",
            stderr="",
        )
        with mock.patch("subprocess.run", return_value=completed):
            _, value, error = cli.fetch_snapshot("dev-api", 3, 20)
        self.assertEqual(value, {"runs": []})
        self.assertIsNone(error)


if __name__ == "__main__":
    unittest.main()
