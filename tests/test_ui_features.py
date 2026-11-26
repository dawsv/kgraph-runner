"""Verify the Hero Town shell and its optional live feature configuration."""

import json
from pathlib import Path
import re
import shutil
import subprocess
import unittest

from kgraph_runner import render_town_html


class TownFeatureTests(unittest.TestCase):
    snapshot = {"title": "Feature test", "nodes": [], "edges": []}

    def options(self, **kwargs):
        html = render_town_html(self.snapshot, **kwargs)
        match = re.search(r'<script type="application/json" id="town-options">(.*?)</script>', html)
        self.assertIsNotNone(match)
        return json.loads(match.group(1))

    def test_optional_feature_endpoints(self):
        for endpoints in (
            {"events": "/api/events", "logs": "/api/logs", "qa": "/api/qa"},
            {"events": "/api/events", "logs": "/api/runs", "qa": "/api/runs"},
            {"events": None, "logs": None, "qa": None},
        ):
            with self.subTest(endpoints=endpoints):
                options = self.options(**{f"{key}_url": url for key, url in endpoints.items() if url})
                self.assertEqual({key: options[key] for key in endpoints}, endpoints)
                self.assertIsNone(options["respond"])

    def test_read_only_option_defaults_to_false(self):
        self.assertIs(self.options(runs_url="/api/runs")["readOnly"], False)
        self.assertIs(self.options(runs_url="/api/runs", read_only=True)["readOnly"], True)

    def test_static_shell_hides_live_panels_and_preserves_inspector_order(self):
        html = render_town_html(self.snapshot)
        self.assertIn('<section id="inspect-log" class="inspect-log" hidden', html)
        self.assertIn('<details class="qa-panel" id="qa-panel" hidden>', html)
        self.assertRegex(html, r"\.inspector-head \{[^}]*position: sticky")
        self.assertRegex(
            html,
            r'(?s)class="inspector-head".*?id="open-subgraph".*?id="inspect-subtasks".*?id="inspect-fields"'
            r'.*?id="inspect-outputs".*?id="inspect-log".*?</aside>',
        )
        self.assertNotIn("/artifacts/", html)

    def test_town_script_syntax(self):
        node = shutil.which("node")
        if node is None:
            self.skipTest("Node.js is unavailable")
        script = Path(__file__).resolve().parents[1] / "src" / "kgraph_runner" / "ui_assets" / "town.js"
        result = subprocess.run([node, "--check", str(script)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
