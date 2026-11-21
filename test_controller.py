import unittest
from unittest.mock import Mock, patch
from urllib.error import HTTPError

from controller import Controller, launch, uses_pool


class PollingTest(unittest.TestCase):
    def test_workflow_matching(self):
        slots = ["small", "large-compute"]
        for directive, expected in (
            ("ubuntu-latest", False),
            ("[self-hosted, small]", True),
            ("[self-hosted, unknown]", False),
            ("'${{ matrix.runner }}'", True),
            ("{labels: [self-hosted, large-compute]}", True),
        ):
            with self.subTest(directive=directive):
                source = f"jobs:\n  build:\n    runs-on: {directive}\n"
                self.assertEqual(uses_pool(source, slots), expected)

    @patch("controller.containers", return_value=set())
    @patch("controller.time.monotonic")
    def test_active_jobs_polled_separately_from_run_discovery(self, clock, _containers):
        app = Mock()
        app.repositories.return_value = [
            {"full_name": name, "archived": False, "disabled": False}
            for name in ("owner/active", "owner/idle", "owner/hosted")
        ]
        app.workflow_files.side_effect = lambda repository: [repository]
        app.workflow_source.side_effect = lambda repository, _: (
            "jobs: {build: {runs-on: ubuntu-latest}}" if repository.endswith("hosted")
            else "jobs: {build: {runs-on: [self-hosted, small]}}"
        )
        app.active_runs.side_effect = lambda repository: {(1, 1)} if repository.endswith("active") else set()
        app.run_jobs.return_value = []
        config = {"instance": "test", "poll_seconds": 60, "slots": [{"name": "small", "label": "small"}]}
        controller = Controller(app, config)

        for now in (0, 9, 10, 20, 30, 40, 50):
            clock.return_value = now
            controller.cycle()
        self.assertEqual(app.repositories.call_count, 1)
        self.assertEqual(app.active_runs.call_count, 2)
        self.assertEqual(app.run_jobs.call_count, 6)
        self.assertEqual(controller.repositories_cache, ["owner/active", "owner/idle"])

        clock.return_value = 60
        controller.cycle()
        self.assertEqual(app.active_runs.call_count, 4)
        self.assertEqual(app.run_jobs.call_count, 7)

        clock.return_value = 600
        controller.cycle()
        self.assertEqual(app.repositories.call_count, 2)
        self.assertEqual(app.workflow_source.call_count, 3)

    def test_private_workflow_metadata_fallback(self):
        app = Mock()
        app.workflow_files.side_effect = HTTPError("https://api.github.com", 403, "Forbidden", {}, None)
        app.has_active_workflows.return_value = True
        controller = Controller(app, {"slots": [{"label": "small"}]})
        self.assertTrue(controller.repository_uses_pool("owner/private"))
        app.has_active_workflows.assert_called_once_with("owner/private")

    def test_private_workflow_directory_hidden_as_not_found(self):
        app = Mock()
        app.workflow_files.return_value = []
        app.has_active_workflows.return_value = True
        controller = Controller(app, {"slots": [{"label": "small"}]})
        self.assertTrue(controller.repository_uses_pool("owner/private", private=True))
        self.assertFalse(controller.repository_uses_pool("owner/public"))

    @patch("controller.subprocess.Popen")
    def test_cargo_jobs_default_is_slot_specific(self, start_process):
        for name, settings in (("small", {}), ("large", {"cargo_build_jobs": 8})):
            with self.subTest(slot=name):
                slot = {"cpus": 2, "memory": "4g", **settings}
                launch("docker", "image", "test", slot, name, "placeholder")
                args = start_process.call_args.args[0]
                self.assertEqual("CARGO_BUILD_JOBS=8" in args, bool(settings))
