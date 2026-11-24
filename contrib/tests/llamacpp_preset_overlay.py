#!/usr/bin/env python3
"""The reference presets overlay script on temporary INI files: hand-written
sections and comments kept, managed keys marked, variants copied from their
base, pinned keys refused, atomic and idempotent writes."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "llamacpp-preset-overlay.py"

BASE = """version = 1

; shared by every model
[*]
c = 8192
; switchback-pinned: n-gpu-layers

; the big one
[qwen3-35b]
model = /models/qwen3-35b.gguf
ctx-size = 32768
; switchback-pinned: cache-type-k
cache-type-k = f16

[small]
model = /models/small.gguf
"""


class Overlay(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="preset-overlay-")
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.base = self.dir / "base.ini"
        self.out = self.dir / "models.ini"
        self.base.write_text(BASE)

    def run_script(self, overlay):
        return subprocess.run([sys.executable, "-I", str(SCRIPT), "--base", str(self.base),
                               "--out", str(self.out)], input=json.dumps(overlay),
                              capture_output=True, text=True, timeout=30)

    def test_managed_keys_replace_and_append_keeping_comments(self):
        run = self.run_script({"models": {
            "qwen3-35b": {"settings": {"ctx-size": "65536", "flash-attn": "on"}}}})
        self.assertEqual(run.returncode, 0, run.stderr)
        text = self.out.read_text()
        self.assertIn("; shared by every model", text)
        self.assertIn("; the big one", text)
        self.assertIn("model = /models/qwen3-35b.gguf", text)
        self.assertNotIn("ctx-size = 32768", text)
        self.assertIn("; managed by switchback\nctx-size = 65536", text)
        self.assertIn("; managed by switchback\nflash-attn = on", text)
        # The other section is untouched.
        self.assertIn("[small]\nmodel = /models/small.gguf", text)
        # The base file itself never changes.
        self.assertEqual(self.base.read_text(), BASE)

    def test_variant_copies_its_base_and_adds_its_keys(self):
        run = self.run_script({"models": {
            "qwen3-35b": {"settings": {"ctx-size": "65536"}},
            "qwen3-35b~1a2b3c4d": {"base": "qwen3-35b", "settings": {"ctx-size": "131072"}}}})
        self.assertEqual(run.returncode, 0, run.stderr)
        text = self.out.read_text()
        variant = text.split("[qwen3-35b~1a2b3c4d]", 1)[1]
        self.assertIn("model = /models/qwen3-35b.gguf", variant)
        self.assertIn("cache-type-k = f16", variant)
        self.assertIn("ctx-size = 131072", variant)
        self.assertNotIn("ctx-size = 32768", variant)

    def test_pinned_keys_are_refused_and_out_is_left_alone(self):
        self.out.write_text("previous\n")
        for overlay in ({"models": {"qwen3-35b": {"settings": {"cache-type-k": "q8_0"}}}},
                        {"models": {"small": {"settings": {"n-gpu-layers": "10"}}}}):
            run = self.run_script(overlay)
            self.assertEqual(run.returncode, 2, overlay)
            self.assertIn("pinned", run.stderr)
            self.assertEqual(self.out.read_text(), "previous\n")
        # The pinned value itself is fine.
        run = self.run_script({"models": {"qwen3-35b": {"settings": {"cache-type-k": "f16"}}}})
        self.assertEqual(run.returncode, 0, run.stderr)

    def test_bad_input_is_refused(self):
        for overlay in ({"models": {"m": {"settings": {"a b": "1"}}}},
                        {"models": {"m": {"settings": {"k": "1\n[evil]"}}}},
                        {"models": []}):
            run = self.run_script(overlay)
            self.assertEqual(run.returncode, 2, overlay)
        self.assertFalse(self.out.exists())

    def test_a_section_the_base_lacks_is_refused(self):
        # A typo, or a variant of a model the base file lacks, would become
        # a section without `model =`.
        for overlay in ({"models": {"qwen-typo": {"settings": {"ctx-size": "4096"}}}},
                        {"models": {"nope~1a2b3c4d": {"base": "nope",
                                                      "settings": {"ctx-size": "4096"}}}},
                        {"models": {"small~1a2b3c4d": {"settings": {"ctx-size": "4096"}}}}):
            run = self.run_script(overlay)
            self.assertEqual(run.returncode, 2, overlay)
            self.assertIn("base file", run.stderr)
        self.assertFalse(self.out.exists())
        # A variant of a base section is fine.
        run = self.run_script({"models": {"small~1a2b3c4d": {
            "base": "small", "settings": {"ctx-size": "8192"}}}})
        self.assertEqual(run.returncode, 0, run.stderr)

    def test_same_overlay_twice_is_unchanged(self):
        overlay = {"models": {"small": {"settings": {"ctx-size": "4096"}}}}
        self.assertEqual(self.run_script(overlay).returncode, 0)
        first = self.out.read_text()
        run = self.run_script(overlay)
        self.assertEqual(run.returncode, 0)
        self.assertEqual(run.stdout.strip(), "unchanged")
        self.assertEqual(self.out.read_text(), first)
        # An empty overlay gives back the hand-written file.
        self.assertEqual(self.run_script({"models": {}}).returncode, 0)
        self.assertEqual(self.out.read_text(), BASE)
        leftovers = [p.name for p in self.dir.iterdir() if p.name.startswith(".preset-")]
        self.assertEqual(leftovers, [])


    def run_with_state(self, overlay):
        state = self.dir / "state"
        state.mkdir(exist_ok=True)
        return subprocess.run([sys.executable, "-I", str(SCRIPT), "--base", str(self.base),
                               "--out", str(self.out), "--state-dir", str(state)],
                              input=json.dumps(overlay), capture_output=True, text=True,
                              timeout=30)

    def test_show_prints_the_base_keys_and_pins_and_writes_nothing(self):
        run = subprocess.run([sys.executable, "-I", str(SCRIPT), "--base", str(self.base),
                              "--show"], capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        shown = json.loads(run.stdout)
        self.assertEqual(shown["global"], {"keys": {"c": "8192"}, "pinned": ["n-gpu-layers"]})
        self.assertEqual(shown["models"]["qwen3-35b"]["keys"],
                         {"model": "/models/qwen3-35b.gguf", "ctx-size": "32768",
                          "cache-type-k": "f16"})
        self.assertEqual(shown["models"]["qwen3-35b"]["pinned"], ["cache-type-k"])
        self.assertEqual(shown["models"]["small"], {"keys": {"model": "/models/small.gguf"},
                                                    "pinned": []})
        self.assertNotIn("*", shown["models"])
        self.assertFalse(self.out.exists())
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()), ["base.ini"])

    def test_two_lanes_on_one_engine_keep_each_others_settings(self):
        run = self.run_with_state({"lane": "big", "models": {
            "qwen3-35b": {"settings": {"ctx-size": "65536"}}}})
        self.assertEqual(run.returncode, 0, run.stderr)
        run = self.run_with_state({"lane": "light", "models": {
            "small": {"settings": {"ctx-size": "4096"}}}})
        self.assertEqual(run.returncode, 0, run.stderr)
        text = self.out.read_text()
        # The second lane's write kept the first lane's setting.
        self.assertIn("ctx-size = 65536", text)
        self.assertIn("[small]\nmodel = /models/small.gguf\n; managed by switchback\nctx-size = 4096",
                      text)
        # A lane's empty overlay takes only its own settings off.
        run = self.run_with_state({"lane": "light", "models": {}})
        self.assertEqual(run.returncode, 0, run.stderr)
        text = self.out.read_text()
        self.assertIn("ctx-size = 65536", text)
        self.assertNotIn("ctx-size = 4096", text)
        saved = json.loads((self.dir / "state" / "big.json").read_text())
        self.assertEqual(saved["models"]["qwen3-35b"]["settings"], {"ctx-size": "65536"})

    def test_two_lanes_that_disagree_are_refused_and_nothing_is_saved(self):
        self.assertEqual(self.run_with_state({"lane": "big", "models": {
            "small": {"settings": {"ctx-size": "4096"}}}}).returncode, 0)
        before = self.out.read_text()
        run = self.run_with_state({"lane": "light", "models": {
            "small": {"settings": {"ctx-size": "8192"}}}})
        self.assertEqual(run.returncode, 2)
        self.assertIn("lanes big and light set ctx-size differently", run.stderr)
        self.assertEqual(self.out.read_text(), before)
        self.assertFalse((self.dir / "state" / "light.json").exists())
        # The same value from both lanes is fine.
        run = self.run_with_state({"lane": "light", "models": {
            "small": {"settings": {"ctx-size": "4096"}}}})
        self.assertEqual(run.returncode, 0, run.stderr)

    def test_a_pinned_key_refused_with_state_saves_nothing(self):
        run = self.run_with_state({"lane": "big", "models": {
            "qwen3-35b": {"settings": {"cache-type-k": "q8_0"}}}})
        self.assertEqual(run.returncode, 2)
        self.assertIn("pinned", run.stderr)
        self.assertFalse(self.out.exists())
        self.assertEqual(list((self.dir / "state").iterdir()), [])

    def test_state_needs_a_lane_name(self):
        for overlay in ({"models": {}}, {"lane": "../x", "models": {}}):
            run = self.run_with_state(overlay)
            self.assertEqual(run.returncode, 2, overlay)
            self.assertIn("lane", run.stderr)


if __name__ == "__main__":
    unittest.main()
