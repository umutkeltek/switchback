#!/usr/bin/env python3
"""sb ai doctor, sb ai fit --store, the new `sb ai set` keys and sb ai tune,
against a fake gateway on a local port.

The fake's engine is fast and deterministic enough to tune: its decode and
prompt speed follow the load settings it was loaded with (each setting has
an effect factor), with a seeded jitter of 2% on decode and 1% on time to
first token, and, when a test sets it, a seeded drift of decode speed
between runs (`run_sd`, or a fixed `drift_plan`); it keeps the slot count
(concurrent requests beyond it wait), answers needles unless a setting
breaks them, changes its greedy answer when a setting is lossy, and fails
loads it is told to fail. Every run goes through the real `sb` dispatcher
in a child process."""

import argparse
import io
import json
import os
import random
import statistics
import sys
import re
import signal
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
KEY = "sk-test-77aa01cd"
TARGET = "box/m"
PRIMES = "2, 3, 5, 7, 11, 13, 17, 19, 23, 29"


class Engine:
    """A llama.cpp router's one model, as seen through the gateway."""

    def __init__(self, seed=11):
        self.rng = random.Random(seed)
        self.rng_lock = threading.Lock()
        # Decode jitter. Well under the 5% noise floor between two
        # controls' medians, so a run judged on decode is noisy only when a
        # test makes it so (`run_sd`, `drift_plan`).
        self.spread = 0.02
        self.base_decode = 400.0      # tok/s; a 64-token answer takes ~160 ms, so scheduling noise stays near 1%
        self.base_ttft_ms = 200.0     # long enough that scheduling noise stays under 1%
        self.base_prefill = 60000.0   # tok/s
        # Run-to-run drift, off unless a test sets it: a real machine's
        # speed moves between runs (heat, other work) more than within one,
        # and a run's median does not narrow it. The tuner opens every run
        # with one 64-token warm-up request; there the fake draws a new
        # decode factor around 1 with standard deviation `run_sd` (seeded:
        # a test sees the same draws every time), or takes the next factor
        # of `drift_plan` (the last one stays), for a known noise.
        self.run_sd = 0.0
        self.drift_plan = []
        self.drift = 1.0
        self.drift_rng = random.Random(seed + 1)
        self.effects = {}             # (key, json value) -> {"decode": f, "ttft": f, "prefill": f}
        self.repeat_effects = {}      # (key, json value) -> [decode factor per apply]
        self.apply_factor = 1.0
        self.loaded = {}
        self.needle_breaks = set()    # (key, json value) that lose needles
        self.greedy_breaks = set()    # (key, json value) that change the greedy answer
        self.load_fails = set()       # (key, json value) that fail to load
        self.load_error = None        # the message of a failed load (default: generic)
        self.soak_breaks = False      # concurrent long requests fail at parallel > 1
        self.slots = threading.Semaphore(1)
        self.slot_count = 1
        self.requests = []            # (kind, completed)
        self.applies = []             # the load of every apply

    def factor(self, metric):
        out = self.apply_factor * self.drift if metric == "decode" else 1.0
        for key, value in self.loaded.items():
            effect = self.effects.get((key, json.dumps(value)))
            if effect:
                out *= effect.get(metric, 1.0)
        return out

    def new_run(self):
        with self.rng_lock:
            if self.drift_plan:
                self.drift = self.drift_plan.pop(0) if len(self.drift_plan) > 1 else self.drift_plan[0]
            elif self.run_sd:
                self.drift = max(0.05, self.drift_rng.gauss(1.0, self.run_sd))

    def jitter(self, spread=None):
        """Seeded run-to-run noise: `spread` (2%) on decode, 1% on time to
        first token, so the noise rules (a side metric against the
        controls, a run noisy on its judged metric) are tested by their own
        cases, not by chance."""
        spread = self.spread if spread is None else spread
        with self.rng_lock:
            return 1.0 + self.rng.uniform(-spread, spread)

    def apply(self, load):
        for key, value in load.items():
            if (key, json.dumps(value)) in self.load_fails:
                return self.load_error or f"could not load with {key}={value}"
        self.loaded = dict(load)
        self.applies.append(dict(load))
        self.apply_factor = 1.0
        for key, value in load.items():
            queue = self.repeat_effects.get((key, json.dumps(value)))
            if queue:
                self.apply_factor = queue.pop(0) if len(queue) > 1 else queue[0]
        slots = int(load.get("parallel") or 1)
        self.slots = threading.Semaphore(slots)
        self.slot_count = slots
        return None

    def broken(self, table):
        return any((key, json.dumps(value)) in table for key, value in self.loaded.items())


class Fake:
    def __init__(self):
        self.engine = Engine()
        self.lock = threading.Lock()
        self.seen = []
        self.doc = {}
        self.revision = 0
        self.history = []
        self.lane_state = "healthy"
        self.lane_in_flight = 0
        self.lane_experiments = True
        self.woken_state = "idle_countdown"
        self.lock_refusal = None       # ("lane_busy" | "quiet_window", message)
        self.locks = {}
        self.holds = {}
        self.ledger = []
        self.build = "b100"
        self.model_file = "file-a"
        # Shaped like the gateway over a real llama.cpp router (b12359): a
        # preset model has no sidecars; speculation comes from the preset
        # or the loaded instance's arguments (`speculative`).
        self.sidecars = []
        self.speculative = {"spec_type": None, "mtp": False, "draft_model": False, "sources": []}
        self.quiet_window = None       # the heartbeat's answer, once set
        self.lose_lock_after = None    # heartbeats before the lock is "lost" (a restart)
        self.beats = 0
        self.busy_applies = 0          # apply-now answers held back (model busy), then pending
        self.pending_polls = 0         # /v1/engines reads the pending apply stays pending
        self.pending_apply = False
        self.paused_chats = 0          # chats answered 503 model_paused first
        self.report_targets = [{"target": TARGET, "ceiling": {"decode_tps": 4000.0},
                                "swaps_last_hour": 0}]
        self.report_findings = []
        self.chat_started = threading.Event()
        self.chats = 0
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    # ------------------------------------------------------------ routes

    def settings_view(self):
        return {"target": TARGET, "revision": self.revision, "durable": True,
                "effective": {}, "profiles": {}, "document": self.doc,
                "load": {"engine": {"lane": "box", "kind": "llama_cpp_router", "status": "loaded"},
                         "settings": {}}}

    def put_settings(self, body):
        if body.get("revision") != self.revision:
            return 409, {"error": {"message": f"settings changed: you edited revision "
                                              f"{body.get('revision')}, the current one is "
                                              f"{self.revision}", "type": "revision_conflict"}}
        body = dict(body)
        apply = body.pop("apply", None)
        note = body.pop("note", None)
        body.pop("revision", None)
        body.pop("wake", None)
        self.doc = body
        self.revision += 1
        action = "put" + (f": {note}" if note else "")
        self.history.append({"revision": self.revision, "action": action, "doc": json.dumps(body)})
        view = self.settings_view()
        if apply == "now" and self.busy_applies > 0:
            # The model still served a request: the write stands, the apply
            # waits for it to be idle (200, pending).
            self.busy_applies -= 1
            self.pending_apply = True
            self.held_load = body.get("load") or {}
            view["apply"] = {"ok": True, "reloaded": False, "reason": "model_busy", "pending": True,
                             "deferred": ["m"], "message": "held back until idle"}
            return 200, view
        if apply == "now":
            problem = self.engine.apply(body.get("load") or {})
            if note and note.endswith(getattr(self, "slow_note", "\x00")):
                self.slow_started = True
                time.sleep(2.0)
            view["apply"] = ({"ok": False, "error": problem, "message": problem} if problem
                             else {"ok": True, "message": "reloaded with the new settings"})
        return 200, view

    def chat(self, handler, body):
        engine = self.engine
        with self.lock:
            paused = self.paused_chats > 0
            if paused:
                self.paused_chats -= 1
        if paused or self.pending_apply:
            handler.send_response(503)
            handler.send_header("Content-Type", "application/json")
            handler.send_header("Retry-After", "5")
            payload = json.dumps({"error": {"message": "`box/m` is being unloaded or reloaded with "
                                                       "new settings", "type": "model_paused"}}).encode()
            handler.send_header("Content-Length", str(len(payload)))
            handler.end_headers()
            handler.wfile.write(payload)
            with self.lock:
                self.engine.requests.append(("paused", False))
            return
        prompt = body["messages"][0]["content"]
        if body.get("max_tokens") == 64 and "You are a careful assistant" in prompt:
            engine.new_run()  # a run's warm-up request
        long_request = len(prompt) > 4000
        completed = False
        with self.lock:
            self.chats += 1
        self.chat_started.set()
        engine.slots.acquire()
        try:
            prompt_tokens = len(prompt) // 4
            ttft = (engine.base_ttft_ms / engine.factor("ttft")
                    + prompt_tokens / (engine.base_prefill * engine.factor("prefill")) * 1000) * engine.jitter(0.01)
            completion = 64 if body.get("max_tokens", 64) >= 64 else body["max_tokens"]
            decode_s = completion / (engine.base_decode * engine.factor("decode")) * engine.jitter()
            if long_request and engine.soak_breaks and engine.slot_count > 1:
                handler.send_response(500)
                handler.send_header("Content-Type", "application/json")
                payload = json.dumps({"error": {"message": "out of memory", "type": "engine_error"}}).encode()
                handler.send_header("Content-Length", str(len(payload)))
                handler.end_headers()
                handler.wfile.write(payload)
                return
            if "NEEDLE-ONE" in prompt:
                codes = re.findall(r"NEEDLE-(?:ONE|TWO) is ([A-Z0-9]{8})\.", prompt)
                if engine.broken(engine.needle_breaks):
                    codes = codes[:1]
                text = "The codes are " + " and ".join(codes) + "."
            elif prompt == "List the first ten prime numbers, separated by commas, and nothing else.":
                text = PRIMES if not engine.broken(engine.greedy_breaks) else "2, 3, 5, 7, 9, 11"
            else:
                text = "The notes list words about rivers, stones and markets in a loose order."
            handler.send_response(200)
            handler.send_header("Content-Type", "text/event-stream")
            handler.end_headers()
            try:
                time.sleep(ttft / 1000)
                handler.wfile.write(b"data: " + json.dumps({"choices": [{"index": 0, "delta": {
                    "content": text[:10]}, "finish_reason": None}]}).encode() + b"\n\n")
                handler.wfile.flush()
                time.sleep(decode_s)
                handler.wfile.write(b"data: " + json.dumps({"choices": [{"index": 0, "delta": {
                    "content": text[10:]}, "finish_reason": "stop"}]}).encode() + b"\n\n")
                handler.wfile.write(b"data: " + json.dumps({"choices": [], "usage": {
                    "prompt_tokens": prompt_tokens, "completion_tokens": completion,
                    "total_tokens": prompt_tokens + completion}}).encode() + b"\n\n")
                handler.wfile.write(b"data: [DONE]\n\n")
                handler.wfile.flush()
                completed = True
            except (BrokenPipeError, ConnectionResetError):
                pass
        finally:
            engine.slots.release()
            with self.lock:
                engine.requests.append(("long" if long_request else "chat", completed))

    def route(self, method, path, body):
        if method == "GET" and path == f"/v1/models/{TARGET}/settings":
            return 200, {"model": TARGET, "targets": [self.settings_view()], "revision": self.revision}
        if method == "PUT" and path == f"/v1/models/{TARGET}/settings":
            return self.put_settings(body)
        if method == "GET" and path == f"/v1/models/{TARGET}/settings/history":
            return 200, {"target": TARGET, "revision": self.revision, "history": self.history[::-1]}
        if method == "POST" and path in (f"/v1/models/{TARGET}/load", f"/v1/models/{TARGET}/unload"):
            if path.endswith("/load"):
                time.sleep(0.06 if self.engine.loaded.get("mmap") is False else 0.02)
            return 200, {"target": TARGET, "status": "ok"}
        if method == "GET" and path == "/v1/workloads/capacity":
            return 200, {"object": "list", "data": [
                {"name": "box", "host": "gpu-box", "state": self.lane_state, "level": "ok",
                 "queue_depth": 0, "in_flight": self.lane_in_flight,
                 "experiments": self.lane_experiments}]}
        if method == "POST" and path == "/v1/hosts/gpu-box/holds":
            hold_id = f"hold-{len(self.holds) + 1}"
            self.holds[hold_id] = body
            if body.get("wake"):
                # The real gateway: a machine woken by a hold, with no request
                # yet, counts down to idle while the hold keeps it on.
                self.lane_state = self.woken_state
            return 201, {"id": hold_id, "host": "gpu-box", "state": "active"}
        if method == "DELETE" and path.startswith("/v1/hosts/gpu-box/holds/"):
            hold_id = path.rsplit("/", 1)[1]
            self.holds.pop(hold_id, None)
            return 200, {"id": hold_id, "deleted": True}
        if method == "POST" and path == "/v1/optimize/locks":
            if self.lock_refusal:
                reason, message = self.lock_refusal
                return 409, {"error": {"message": message, "type": reason, "reason": reason}}
            lock_id = f"tune-lock-{len(self.locks) + 1}"
            self.locks[lock_id] = body
            return 201, {"id": lock_id, "target": body["target"], "lane": "box"}
        if method == "PATCH" and path.startswith("/v1/optimize/locks/"):
            lock_id = path.rsplit("/", 1)[1]
            self.beats += 1
            if self.lose_lock_after is not None and self.beats > self.lose_lock_after:
                self.locks.pop(lock_id, None)
            if lock_id not in self.locks:
                return 404, {"error": {"message": f"no tuning lock {lock_id}", "type": "not_found",
                                       "reason": "not_found"}}
            return 200, {"id": lock_id, "quiet_window": self.quiet_window}
        if method == "DELETE" and path.startswith("/v1/optimize/locks/"):
            self.locks.pop(path.rsplit("/", 1)[1], None)
            return 200, {"deleted": True}
        if method == "POST" and path == "/v1/optimize/experiments":
            row = dict(body, at_ms=int(time.time() * 1000), id=len(self.ledger) + 1)
            self.ledger.append(row)
            return 201, {"id": row["id"], "durable": True}
        if method == "GET" and path.startswith("/v1/optimize/experiments"):
            return 200, {"object": "list", "data": self.ledger[::-1]}
        if method == "GET" and path.startswith("/v1/optimize/report"):
            return 200, {"object": "optimize.report", "targets": self.report_targets,
                         "findings": self.report_findings, "hosts": []}
        if method == "GET" and path == "/v1/engines":
            if self.pending_apply:
                if self.pending_polls > 0:
                    self.pending_polls -= 1
                else:
                    # The model went idle: the engine re-read the settings.
                    self.engine.apply(self.held_load)
                    self.pending_apply = False
            return 200, {"object": "list", "data": [
                {"lane": "box", "build": self.build, "pending_apply": self.pending_apply,
                 "deferred": ["m"] if self.pending_apply else [],
                 "models": [{"id": "m", "file": self.model_file, "sidecars": self.sidecars,
                             "speculative": self.speculative}]}]}
        return 404, None

    def handler(self):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def serve(self):
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b""
                body = json.loads(raw) if raw else {}
                with fake.lock:
                    fake.seen.append((self.command, self.path, body,
                                      {k.lower(): v for k, v in self.headers.items()}))
                if self.command == "POST" and self.path == "/v1/chat/completions":
                    fake.chat(self, body)
                    return
                if self.command == "PUT" and "revision" not in body and self.headers.get("If-Match"):
                    # A quoted ETag (`"7"`) or a bare number, as the gateway takes.
                    tag = self.headers["If-Match"].strip().removeprefix("W/").strip('"')
                    body = dict(body, revision=int(tag))
                status, payload = fake.route(self.command, self.path, body)
                data = json.dumps(payload).encode() if payload is not None else b""
                self.send_response(status)
                if payload is not None:
                    self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = serve

        return Handler

    def calls(self, method=None, prefix=""):
        with self.lock:
            return [(m, p, b) for m, p, b, _ in self.seen
                    if (method is None or m == method) and p.startswith(prefix)]


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-ai-tune-")
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name).resolve()
        (root / "home").mkdir()
        (root / "runtime/config").mkdir(parents=True)
        (root / "runtime/manifest.json").write_text("{}")
        (root / "xdg/switchback").mkdir(parents=True)
        self.root = root
        self.fake = Fake()
        self.addCleanup(self.fake.close)
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(root / "home"),
            "XDG_CONFIG_HOME": str(root / "xdg"),
            "SWITCHBACK_RUNTIME_ROOT": str(root / "runtime"),
            "SWITCHBACK_AI_URL": self.fake.url,
            "SWITCHBACK_AI_KEY": KEY,
            "SB_AI_POLL_SECS": "0.05",
            "NO_PROXY": "*", "no_proxy": "*",
        }

    def sb(self, *args, timeout=240):
        run = subprocess.run(["zsh", str(SB), "ai", *args], env=self.env, cwd=self.root,
                             capture_output=True, text=True, timeout=timeout)
        self.assertNotIn(KEY, run.stdout + run.stderr)
        return run


class TuneTests(Base):
    def tune(self, *args):
        # Eight samples a run: the noise rule on the other metrics must not
        # trip on the fake's own jitter.
        return self.sb("tune", TARGET, "--budget", "10m", "--json", "--samples", "8", *args)

    def effect(self, key, value, **factors):
        self.fake.engine.effects[(key, json.dumps(value))] = factors

    def verdicts(self):
        return [row for row in self.fake.ledger if row.get("role") == "verdict"]

    def assert_released(self):
        self.assertEqual(self.fake.locks, {}, "the tuning lock was not released")
        self.assertEqual(self.fake.holds, {}, "the hold was not released")

    # ------------------------------------------------------------ A/B/A

    def test_a_clear_win_is_kept_as_a_settings_revision(self):
        self.effect("flash_attn", "on", decode=1.18)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertEqual([g["lever"] for g in result["gains"]], ["flash_attn"], self.verdicts())
        self.assertEqual(self.fake.doc["load"]["flash_attn"], "on")
        # The winner is its own revision, noted with the run id, so revert undoes it.
        self.assertEqual(self.fake.history[-1]["action"], f"put: tune {result['run_id']}")
        self.assertEqual(result["undo"], f"sb ai revert {TARGET} 0")
        verdict = self.verdicts()[-1]
        self.assertEqual((verdict["verdict"], verdict["lever"]), ("accepted", "flash_attn"))
        self.assertEqual(verdict["engine_build"], "b100")
        roles = [row["role"] for row in self.fake.ledger]
        self.assertIn("baseline", roles)
        self.assertIn("candidate", roles)
        # Every inference request carried the run's lock.
        chats = self.fake.calls("POST", "/v1/chat/completions")
        self.assertTrue(chats)
        heads = [h for m, p, b, h in self.fake.seen if p == "/v1/chat/completions"]
        self.assertTrue(all(h.get("x-switchback-tune-lock") == "tune-lock-1" for h in heads))
        self.assertGreaterEqual(result["table"][0]["ceiling"], 4000.0)
        self.assert_released()

    def test_a_worse_candidate_is_rejected_and_the_settings_stay(self):
        self.effect("flash_attn", "on", decode=0.8)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertEqual(result["gains"], [])
        self.assertNotIn("flash_attn", self.fake.doc.get("load") or {})
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "rejected")
        self.assertIn("inside the noise", verdict["reason"])
        self.assert_released()

    def test_a_win_that_costs_another_metric_more_than_its_noise_is_rejected(self):
        # Decode +15%, but the first token comes 30% later.
        self.effect("flash_attn", "on", decode=1.15, ttft=0.70)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "rejected", verdict)
        self.assertIn("ttft_ms got", verdict["reason"])
        self.assertIn("its own noise allows", verdict["reason"])
        self.assertNotIn("flash_attn", self.fake.doc.get("load") or {})

    def test_a_surprising_win_is_repeated_and_a_fluke_is_dropped(self):
        # +60% on the first apply only: the repeat finds nothing.
        self.fake.engine.repeat_effects[("flash_attn", json.dumps("on"))] = [1.6, 1.0, 1.0]
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        candidates = [row for row in self.fake.ledger if row.get("role") == "candidate"]
        self.assertGreaterEqual(len(candidates), 2, "the surprising win was not repeated")
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "rejected", verdict)
        self.assertIn("first run did not hold", verdict["reason"])
        self.assertNotIn("flash_attn", self.fake.doc.get("load") or {})

    def test_a_surprising_win_that_repeats_is_kept(self):
        self.effect("flash_attn", "on", decode=1.5)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        candidates = [row for row in self.fake.ledger if row.get("role") == "candidate"]
        self.assertEqual(len(candidates), 2)
        self.assertEqual(self.verdicts()[-1]["verdict"], "accepted")
        self.assertEqual(self.fake.doc["load"]["flash_attn"], "on")

    def test_a_kv_change_that_loses_needles_is_rejected(self):
        self.effect("cache_type_k", "q8_0", decode=1.15)
        self.fake.engine.needle_breaks.add(("cache_type_k", json.dumps("q8_0")))
        self.fake.doc = {"load": {"ctx_size": 8192}}
        run = self.tune("--levers", "cache_type")
        self.assertEqual(run.returncode, 0, run.stderr)
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "rejected")
        self.assertIn("needle", verdict["reason"])
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 8192})
        longs = [r for r in self.fake.engine.requests if r[0] == "long"]
        self.assertTrue(longs, "no long-context request was sent")

    def test_a_lossy_change_must_keep_the_greedy_answer(self):
        self.effect("flash_attn", "on", decode=1.15)
        self.fake.engine.greedy_breaks.add(("flash_attn", json.dumps("on")))
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("greedy output changed", self.verdicts()[-1]["reason"])

    def test_more_slots_need_a_concurrent_soak(self):
        # The soak is under test here, not the noise rule: 1% jitter keeps
        # the controls' side metrics in step (and a side metric may move by
        # what a win on it would need, its own noise never under 2%, so
        # clock jitter on a busy machine does not reject the change).
        self.fake.engine.spread = 0.01
        self.fake.doc = {"load": {"ctx_size": 4096}}
        run = self.tune("--levers", "parallel", "--concurrency", "1,2")
        self.assertEqual(run.returncode, 0, run.stderr)
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "accepted", verdict)
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 8192, "parallel": 2})
        # The soak: two long requests at once at the full per-slot context.
        longs = [r for r in self.fake.engine.requests if r[0] == "long"]
        self.assertGreaterEqual(len(longs), 2)

    def test_a_failed_soak_rejects_more_slots(self):
        self.fake.doc = {"load": {"ctx_size": 4096}}
        self.fake.engine.soak_breaks = True
        run = self.tune("--levers", "parallel", "--concurrency", "1,2")
        self.assertEqual(run.returncode, 0, run.stderr)
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "rejected")
        self.assertIn("concurrent soak", verdict["reason"])
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 4096})

    def test_draft_length_is_swept_per_concurrency(self):
        self.fake.doc = {"load": {"spec_type": "draft-mtp", "draft_max": 1}}
        self.effect("draft_max", 3, decode=1.35)
        # Twelve samples: the noise rule on time to first token, a metric
        # this lever does not touch, must not trip on the fake's jitter.
        run = self.tune("--levers", "draft_max", "--concurrency", "1,2", "--samples", "12")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        table = result["sweeps"]["draft_max"]
        self.assertEqual(len(table), 4, table)
        self.assertTrue(all(set(per) == {"1", "2"} for per in table.values()), table)
        self.assertEqual(self.fake.doc["load"]["draft_max"], 3, self.verdicts())

    def test_a_failed_load_is_a_failed_trial_not_a_crash(self):
        self.fake.engine.load_fails.add(("flash_attn", json.dumps("on")))
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.verdicts()[-1]["verdict"], "failed")
        self.assertIn("did not load", self.verdicts()[-1]["reason"])
        self.assert_released()

    def test_a_failed_apply_is_a_failed_trial_and_the_run_goes_on(self):
        # The engine refuses the new load setting, as a router does when it
        # is told to load a model whose old process still runs (400 "model
        # is already running"). The trial fails: shown as failed, not as a
        # rejection, not counted as a lever inside the noise, and the run
        # goes on to the next lever.
        self.fake.doc = {"load": {"spec_type": "draft-mtp"}}
        self.fake.engine.load_fails.add(("flash_attn", json.dumps("on")))
        self.fake.engine.load_error = "the engine answered 400 Bad Request: model is already running"
        run = self.tune("--levers", "spec_type,flash_attn,ubatch_size,mmap")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        failed = [row for row in self.verdicts() if row["verdict"] == "failed"]
        self.assertEqual([row["lever"] for row in failed], ["flash_attn"], self.verdicts())
        self.assertIn("model is already running", failed[0]["reason"])
        self.assertEqual([row["lever"] for row in result["failed"]], ["flash_attn"])
        levers = [row["lever"] for row in self.verdicts()]
        self.assertIn("mmap", levers, "the failed trial ended the run early")
        self.assertIn("plateau", result["stopped"])
        self.assertEqual(self.fake.doc["load"], {"spec_type": "draft-mtp"})
        self.assertEqual(self.fake.engine.loaded, {"spec_type": "draft-mtp"})
        self.assert_released()

    def test_text_output_shows_the_noise_and_a_failed_trial(self):
        self.fake.engine.load_fails.add(("flash_attn", json.dumps("on")))
        run = self.sb("tune", TARGET, "--budget", "10m", "--samples", "8", "--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertRegex(run.stdout, r"noise: ±\d+\.\d% \(SD of one run\) on decode between 3 runs")
        self.assertIn('failed (not measured): flash_attn {"flash_attn": "on"}', run.stdout)

    # ------------------------------------------------------------ measured noise

    # The first real tune saw two controls differ by 22% and by 8%: an SD
    # of one run of about 7%. There a real +20% must be kept and a +5% (no
    # better than the noise) rejected.

    def test_with_7_percent_noise_a_20_percent_win_is_kept(self):
        self.fake.engine.run_sd = 0.07
        self.effect("flash_attn", "on", decode=1.20)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertEqual([g["lever"] for g in result["gains"]], ["flash_attn"], self.verdicts())
        self.assertEqual(self.fake.doc["load"]["flash_attn"], "on")
        noise = result["noise"]
        # Over the floor: two more runs, with more samples each.
        self.assertGreater(noise["sd_pct"], 2.0, noise)
        self.assertEqual(noise["runs"], 5, noise)
        self.assertGreater(noise["samples_per_run"], 8, noise)
        self.assertTrue(result["tuned"])
        # A noisy machine: the win held over two candidate runs or more.
        self.assertGreaterEqual(self.verdicts()[-1]["runs"], 2, self.verdicts())
        self.assert_released()

    def test_with_7_percent_noise_a_5_percent_win_is_rejected(self):
        self.fake.engine.run_sd = 0.07
        self.effect("flash_attn", "on", decode=1.05)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertEqual(result["gains"], [], self.verdicts())
        self.assertEqual(self.verdicts()[-1]["verdict"], "rejected")
        self.assertNotIn("flash_attn", self.fake.doc.get("load") or {})
        self.assert_released()

    def test_with_2_percent_noise_a_10_percent_win_is_kept(self):
        self.fake.engine.run_sd = 0.02
        self.effect("flash_attn", "on", decode=1.10)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertEqual([g["lever"] for g in result["gains"]], ["flash_attn"], self.verdicts())
        self.assertLess(result["noise"]["sd_pct"], 5.0, result["noise"])

    def test_with_30_percent_noise_the_run_stops_and_changes_nothing(self):
        # Five noise runs whose SD is 30%.
        self.fake.engine.drift_plan = [1.0, 1.3, 0.7, 1.3, 0.7]
        self.effect("flash_attn", "on", decode=1.40)
        self.fake.doc = {"load": {"ctx_size": 4096}}
        run = self.tune("--levers", "flash_attn,ubatch_size")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertIn("too noisy to tune: noise ±", result["stopped"])
        self.assertFalse(result["tuned"])
        self.assertIn("too noisy to tune", result["reason"])
        # Five runs only pin the SD down so far: anything clearly over the
        # limit (about 30% here) is the right reading.
        self.assertGreater(result["noise"]["sd_pct"], result["noise"]["too_noisy_above_pct"])
        self.assertEqual(self.fake.calls("PUT"), [], "a setting was written")
        self.assertEqual(self.fake.doc, {"load": {"ctx_size": 4096}})
        self.assertEqual(self.fake.engine.applies, [])
        self.assertEqual(self.verdicts(), [])
        self.assert_released()

    def test_noise_widened_over_the_limit_stops_the_run(self):
        # Five noise runs whose SD is about 17%: under the 20% limit as
        # measured, over it once widened for what five runs cannot pin down
        # (about 1.35 times), which is the SD the judge would use.
        self.fake.engine.drift_plan = [1.0, 1.17, 0.83, 1.17, 0.83]
        self.effect("flash_attn", "on", decode=1.40)
        self.fake.doc = {"load": {"ctx_size": 4096}}
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        noise = result["noise"]
        self.assertLess(noise["sd_pct"], noise["too_noisy_above_pct"], noise)
        self.assertIn("too noisy to tune", result["reason"])
        self.assertFalse(result["tuned"])
        self.assertEqual(self.fake.calls("PUT"), [], "a setting was written")
        self.assertEqual(self.fake.doc, {"load": {"ctx_size": 4096}})
        self.assert_released()

    def test_the_printed_noise_is_the_sd_of_one_run(self):
        # Runs at 1.0, 1.25, 0.75, 1.25, 0.75 times the speed: an SD of 25%
        # (the old corrected range printed 75%). Too noisy: the last line
        # says the run did not tune, and the exit code stays 0.
        self.fake.engine.drift_plan = [1.0, 1.25, 0.75, 1.25, 0.75]
        run = self.sb("tune", TARGET, "--budget", "10m", "--samples", "8", "--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        printed = re.search(r"noise: ±(\d+\.\d)% \(SD of one run\) on decode between 5 runs", run.stdout)
        self.assertTrue(printed, run.stdout)
        self.assertAlmostEqual(float(printed.group(1)), 25.0, delta=1.5)
        last = run.stdout.strip().splitlines()[-1]
        self.assertRegex(last, r"^not tuned: too noisy to tune: noise ±2[3-6]\.\d% \(SD of one run\)")

    # ------------------------------------------------------------ ledger

    def test_a_rejected_lever_is_skipped_until_the_engine_build_changes(self):
        self.effect("flash_attn", "on", decode=0.8)
        self.assertEqual(self.tune("--levers", "flash_attn").returncode, 0)
        candidates_before = len([r for r in self.fake.ledger if r["role"] == "candidate"])
        run = self.tune("--levers", "flash_attn")
        result = json.loads(run.stdout)
        self.assertTrue(any("skipped, rejected in" in note for note in result["notes"]), result["notes"])
        self.assertEqual(len([r for r in self.fake.ledger if r["role"] == "candidate"]), candidates_before)
        # A new engine build: tested again.
        self.fake.build = "b200"
        run = self.tune("--levers", "flash_attn")
        result = json.loads(run.stdout)
        self.assertTrue(any("engine build changed" in note for note in result["notes"]), result["notes"])
        self.assertGreater(len([r for r in self.fake.ledger if r["role"] == "candidate"]), candidates_before)

    def test_history_lists_the_verdicts(self):
        self.effect("flash_attn", "on", decode=0.8)
        self.tune("--levers", "flash_attn")
        run = self.sb("tune", TARGET, "--history")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("flash_attn", run.stdout)
        self.assertIn("rejected", run.stdout)

    # ------------------------------------------------------------ safety

    def test_an_asleep_machine_is_not_woken_without_wake(self):
        self.fake.lane_state = "offline"
        run = self.sb("tune", TARGET, "--levers", "flash_attn")
        self.assertEqual(run.returncode, 1)
        self.assertIn("add --wake", run.stderr)
        self.assertEqual(self.fake.calls("POST", "/v1/hosts/"), [])
        self.assertEqual(self.fake.calls("PUT"), [])
        self.assertEqual(self.fake.calls("POST", "/v1/chat/completions"), [])

    def test_wake_holds_the_machine_and_runs(self):
        self.fake.lane_state = "offline"
        self.effect("flash_attn", "on", decode=1.18)
        run = self.tune("--levers", "flash_attn", "--wake")
        self.assertEqual(run.returncode, 0, run.stderr)
        holds = self.fake.calls("POST", "/v1/hosts/gpu-box/holds")
        self.assertEqual(holds[0][2]["wake"], True)
        # The hold itself ends by the next quiet window, before any lock.
        self.assertEqual(holds[0][2]["until_quiet_window"], True)
        self.assert_released()

    def test_wake_runs_when_the_woken_machine_counts_down_to_idle(self):
        # The real gateway reports a machine our hold woke as counting down to
        # idle, not healthy, until a request arrives: the run must not wait for
        # `healthy` and give up.
        self.fake.lane_state = "offline"
        self.fake.woken_state = "idle_countdown"
        self.effect("flash_attn", "on", decode=1.18)
        run = self.tune("--levers", "flash_attn", "--wake")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertNotIn("did not come up", run.stderr)
        self.assert_released()

    def test_a_busy_lane_is_refused(self):
        self.fake.lock_refusal = ("lane_busy", "lane `box` has 1 request(s) in flight or queued")
        run = self.sb("tune", TARGET, "--levers", "flash_attn")
        self.assertEqual(run.returncode, 1)
        self.assertIn("in flight", run.stderr)
        self.assertEqual(self.fake.calls("PUT"), [])
        self.assertEqual(self.fake.holds, {}, "the hold outlived the refusal")

    def test_an_experiments_off_lane_is_refused_before_any_hold(self):
        self.fake.lane_experiments = False
        self.fake.lane_state = "offline"
        for extra in (["--wake"], ["--dry-run"]):
            run = self.tune("--levers", "flash_attn", *extra)
            self.assertEqual(run.returncode, 1, run.stderr)
            self.assertIn("experiments: false", run.stderr)
        self.assertEqual(self.fake.calls("POST", "/v1/hosts"), [])
        self.assertEqual(self.fake.calls("POST", "/v1/optimize/locks"), [])
        self.assertEqual(self.fake.calls("PUT"), [])
        self.assertEqual(self.fake.lane_state, "offline")

    def test_a_quiet_window_is_refused(self):
        self.fake.lock_refusal = ("quiet_window", "inside the quiet window 00:30-03:00 (UTC)")
        run = self.sb("tune", TARGET)
        self.assertEqual(run.returncode, 1)
        self.assertIn("quiet window", run.stderr)
        self.assertEqual(self.fake.calls("POST", "/v1/chat/completions"), [])

    def test_wake_inside_a_quiet_window_never_wakes_the_machine(self):
        self.fake.lane_state = "offline"
        self.fake.lock_refusal = ("quiet_window", "inside quiet window 00:30-03:00 (UTC)")
        run = self.tune("--levers", "flash_attn", "--wake")
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("quiet window", run.stderr)
        holds = self.fake.calls("POST", "/v1/hosts/")
        self.assertFalse(any(body.get("wake") for _, _, body, *_ in holds), holds)
        self.assertEqual(self.fake.lane_state, "offline")
        self.assertEqual(self.fake.calls("PUT"), [])
        self.assertEqual(self.fake.calls("POST", "/v1/chat/completions"), [])
        self.assert_released()

    def test_a_locked_load_key_is_refused(self):
        self.fake.doc = {"load": {"flash_attn": "off"}, "locked": ["load.flash_attn"]}
        run = self.sb("tune", TARGET, "--levers", "flash_attn")
        self.assertEqual(run.returncode, 1)
        self.assertIn("locks load.flash_attn", run.stderr)
        self.assertEqual(self.fake.calls("POST"), [])
        # Without naming it, the lever is skipped and said.
        run = self.sb("tune", TARGET, "--dry-run", "--json")
        plan = json.loads(run.stdout)
        self.assertNotIn("flash_attn", [row["lever"] for row in plan["levers"]])
        self.assertTrue(any("load.flash_attn is locked" in note for note in plan["notes"]))

    def test_mtp_comes_from_the_preset_or_args_not_sidecars(self):
        # A real router lists no sidecars for a preset model; the loaded
        # instance's arguments say MTP is on: the lever tries it off.
        self.fake.speculative = {"spec_type": "draft-mtp", "mtp": True, "draft_model": False,
                                 "sources": ["args"]}
        run = self.sb("tune", TARGET, "--levers", "spec_type,draft_max", "--dry-run", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        plan = {row["lever"]: row for row in json.loads(run.stdout)["levers"]}
        self.assertEqual(plan["spec_type"]["candidates"], [{"spec_type": "none"}])
        self.assertIsNone(plan["draft_max"]["skipped"], plan["draft_max"])
        # Nothing known (no preset key, no args, no sidecars): skipped.
        self.fake.speculative = {"spec_type": None, "mtp": False, "draft_model": False,
                                 "sources": []}
        run = self.sb("tune", TARGET, "--levers", "spec_type,draft_max", "--dry-run", "--json")
        plan = {row["lever"]: row for row in json.loads(run.stdout)["levers"]}
        self.assertEqual(plan["spec_type"]["candidates"], [])
        self.assertIn("no MTP head", plan["spec_type"]["skipped"])
        self.assertIn("off", plan["draft_max"]["skipped"])
        # A draft model the settings layer knows: draft-simple.
        self.fake.speculative = {"spec_type": None, "mtp": False, "draft_model": True,
                                 "sources": ["settings"]}
        run = self.sb("tune", TARGET, "--levers", "spec_type", "--dry-run", "--json")
        plan = {row["lever"]: row for row in json.loads(run.stdout)["levers"]}
        self.assertEqual(plan["spec_type"]["candidates"], [{"spec_type": "draft-simple"}])

    def test_writes_name_their_revision_with_a_quoted_if_match(self):
        self.effect("flash_attn", "on", decode=1.6)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        puts = [h for m, p, b, h in self.fake.seen if m == "PUT"]
        self.assertTrue(puts)
        for headers in puts:
            self.assertRegex(headers.get("if-match", ""), r'^"\d+"$')

    def test_a_held_back_apply_is_waited_for_not_taken_as_applied(self):
        self.effect("flash_attn", "on", decode=1.6)
        self.fake.busy_applies = 1
        self.fake.pending_polls = 3
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        # No request reached the model while the apply was pending: a chat
        # during it would have been answered 503 model_paused.
        self.assertNotIn(("paused", False), self.fake.engine.requests)
        self.assertEqual(self.fake.pending_polls, 0)
        self.assertFalse(self.fake.pending_apply)
        self.assertEqual(self.verdicts()[-1]["verdict"], "accepted")
        self.assert_released()

    def test_an_apply_that_stays_pending_stops_the_run(self):
        self.effect("flash_attn", "on", decode=1.6)
        self.fake.busy_applies = 1
        self.fake.pending_polls = 10 ** 6
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 1)
        self.assertIn("still not applied", run.stderr + run.stdout)
        self.assert_released()

    def test_a_paused_model_is_asked_again_later(self):
        self.effect("flash_attn", "on", decode=1.6)
        self.fake.paused_chats = 3
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.fake.paused_chats, 0)
        self.assertEqual(self.verdicts()[-1]["verdict"], "accepted", self.verdicts()[-1].get("reason"))

    def test_a_quiet_window_that_begins_stops_the_run_safely(self):
        self.effect("flash_attn", "on", decode=1.6)
        self.fake.quiet_window = "00:30-03:00 Asia/Tokyo"
        run = self.tune("--levers", "flash_attn,ubatch_size")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertIn("quiet window 00:30-03:00 Asia/Tokyo began", result["stopped"])
        # The settings before it are applied again, and nothing was left.
        self.assertEqual(self.fake.doc.get("load") or {}, {})
        self.assertEqual(self.fake.engine.loaded, {})
        self.assert_released()

    def test_a_lost_lock_stops_the_run_safely(self):
        self.effect("flash_attn", "on", decode=1.6)
        self.fake.lose_lock_after = 1
        run = self.tune("--levers", "flash_attn,ubatch_size")
        self.assertEqual(run.returncode, 1, run.stdout)
        result = json.loads(run.stdout)
        self.assertIn("tuning lock is gone", result["stopped"])
        self.assertEqual(self.fake.holds, {}, "the hold was not released")
        best = self.fake.doc.get("load") or {}
        self.assertEqual(self.fake.engine.loaded, best, "the best settings are not applied")

    def test_dry_run_changes_nothing(self):
        # The sidecar fallback (cached Hugging Face models) still works.
        self.fake.sidecars = ["model-mtp.gguf"]
        run = self.sb("tune", TARGET, "--dry-run")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("spec_type", run.stdout)
        self.assertIn("draft-mtp", run.stdout)
        self.assertEqual(self.fake.calls("POST"), [])
        self.assertEqual(self.fake.calls("PUT"), [])

    def test_ctrl_c_restores_the_best_settings_and_interrupts_no_request(self):
        self.fake.doc = {"load": {"ctx_size": 4096}}
        self.effect("flash_attn", "on", decode=1.2)
        proc = subprocess.Popen(["zsh", str(SB), "ai", "tune", TARGET, "--levers", "flash_attn",
                                 "--samples", "12"],
                                env=self.env, cwd=self.root, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        # Interrupt once the candidate is applied and being measured.
        deadline = time.time() + 60
        while time.time() < deadline:
            if any(b.get("note", "").endswith("trial flash_attn") for m, p, b in self.fake.calls("PUT")):
                break
            time.sleep(0.02)
        time.sleep(0.15)
        proc.send_signal(signal.SIGINT)
        out, err = proc.communicate(timeout=60)
        self.assertEqual(proc.returncode, 130, err)
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 4096}, "the best settings were not restored")
        self.assertIn("restore", self.fake.history[-1]["action"])
        self.assertTrue(all(done for _, done in self.fake.engine.requests),
                        "a request was cut off mid-answer")
        self.assert_released()

    def test_sigterm_restores_the_best_settings_and_lets_go(self):
        self.fake.doc = {"load": {"ctx_size": 4096}}
        self.effect("flash_attn", "on", decode=1.2)
        proc = subprocess.Popen(["zsh", str(SB), "ai", "tune", TARGET, "--levers", "flash_attn",
                                 "--samples", "12"],
                                env=self.env, cwd=self.root, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        deadline = time.time() + 60
        while time.time() < deadline:
            if any(b.get("note", "").endswith("trial flash_attn") for m, p, b in self.fake.calls("PUT")):
                break
            time.sleep(0.02)
        time.sleep(0.15)
        proc.send_signal(signal.SIGTERM)
        out, err = proc.communicate(timeout=60)
        self.assertEqual(proc.returncode, 130, err)
        self.assertIn("interrupted (SIGTERM)", out + err)
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 4096}, "the best settings were not restored")
        self.assertIn("restore", self.fake.history[-1]["action"])
        self.assertTrue(all(done for _, done in self.fake.engine.requests),
                        "a request was cut off mid-answer")
        self.assert_released()

    def test_rr_ctrl_c_during_a_trial_apply_restores_the_best_settings(self):
        self.fake.doc = {"load": {"ctx_size": 4096}}
        self.fake.slow_note = "trial flash_attn"
        self.fake.slow_started = False
        self.effect("flash_attn", "on", decode=1.2)
        proc = subprocess.Popen(["zsh", str(SB), "ai", "tune", TARGET, "--levers", "flash_attn",
                                 "--samples", "6"],
                                env=self.env, cwd=self.root, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        deadline = time.time() + 60
        while time.time() < deadline and not self.fake.slow_started:
            time.sleep(0.02)
        time.sleep(0.3)
        proc.send_signal(signal.SIGINT)
        out, err = proc.communicate(timeout=60)
        time.sleep(2.5)
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 4096},
                         "Ctrl-C during the trial's apply left the candidate in place: " + err[-600:])

    def test_plateau_stops_after_three_flat_levers(self):
        self.fake.doc = {"load": {"spec_type": "draft-mtp"}}
        run = self.tune("--levers", "spec_type,flash_attn,ubatch_size,mmap")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        self.assertIn("plateau", result["stopped"], self.verdicts())
        levers = {row["lever"] for row in self.verdicts()}
        self.assertNotIn("mmap", levers, "the run went on past the plateau")


class DoctorAndFitTests(Base):
    """`sb ai doctor`, `sb ai fit --store` and the new `sb ai set` keys."""

    def test_doctor_prints_targets_and_findings(self):
        self.fake.report_targets = [{"target": TARGET, "decode_tps_p50": 80.0, "prompt_tps_p50": 900.0,
                                     "ttft_ms_p50": 210.0, "pct_of_ceiling": 31.0, "swaps_last_hour": 9,
                                     "cache": {"token_hit_pct": 2.0}, "empty_after_thinking": 3}]
        self.fake.report_findings = [{"target": TARGET, "kind": "swap_thrash",
                                      "evidence": "9 model swaps to box/m in the last hour",
                                      "action": "point both names at one model"}]
        run = self.sb("doctor")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("box/m", run.stdout)
        self.assertIn("31%", run.stdout)
        self.assertIn("9 model swaps", run.stdout)
        self.assertIn("-> point both names at one model", run.stdout)
        run = self.sb("doctor", TARGET, "--json")
        self.assertEqual(json.loads(run.stdout)["findings"][0]["kind"], "swap_thrash")
        self.assertTrue(any(p.startswith("/v1/optimize/report?model=box%2Fm")
                            for _, p, _ in self.fake.calls("GET")))

    def test_set_writes_document_keys_and_load_locks(self):
        run = self.sb("set", TARGET, "reasoning=true", "reasoning_guard.min_tokens=512")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.fake.doc["reasoning"], True)
        self.assertEqual(self.fake.doc["reasoning_guard"], {"min_tokens": 512})
        self.assertNotIn("defaults", self.fake.doc)
        # A request setting is still a request setting.
        self.sb("set", TARGET, "reasoning.enabled=false")
        self.assertEqual(self.fake.doc["defaults"]["reasoning"], {"enabled": False})
        self.assertEqual(self.fake.doc["reasoning"], True)
        run = self.sb("set", TARGET, "ctx_size=8192", "--load", "--lock")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("load.ctx_size", self.fake.doc["locked"])
        self.sb("unset", TARGET, "reasoning_guard")
        self.assertNotIn("reasoning_guard", self.fake.doc)

    def test_fit_store_writes_the_ceiling(self):
        sys_path = str(CLI)
        import sys
        if sys_path not in sys.path:
            sys.path.insert(0, sys_path)
        import ai_gateway
        report = {"repo": "org/model", "options": [
            {"quant": "Q4_K_M", "bytes_per_token": 4_000_000_000, "bytes_per_token_source": "file size"},
            {"quant": "Q8_0", "bytes_per_token": 8_000_000_000, "bytes_per_token_source": "file size"}],
            "machines": [{"quant": "Q8_0", "bandwidth_gbs": 800.0}]}
        ceiling = ai_gateway.ceiling_from_fit(report)
        self.assertEqual(ceiling["bytes_per_token"], 8_000_000_000)
        self.assertEqual(ceiling["decode_tps"], 100.0)
        self.assertIn("org/model Q8_0 at 800 GB/s", ceiling["source"])
        ceiling = ai_gateway.ceiling_from_fit(report, quant="q4_k_m", bandwidth=400)
        self.assertEqual((ceiling["bytes_per_token"], ceiling["decode_tps"]), (4_000_000_000, 100.0))
        with self.assertRaises(ai_gateway.Fail):
            ai_gateway.ceiling_from_fit(report, quant="Q2")

    def test_fit_store_end_to_end_through_the_gateway(self):
        # The fake also answers the model index: a dense safetensors model.
        fake = self.fake
        original = fake.route

        def route(method, path, body):
            if path.startswith("/api/models/org/dense"):
                return 200, {"pipeline_tag": "text-generation",
                             "siblings": [{"rfilename": "model.safetensors", "size": 2_000_000_000},
                                          {"rfilename": "config.json", "size": 100}]}
            if path.endswith("/resolve/main/config.json"):
                return 200, {"num_hidden_layers": 24, "num_attention_heads": 16,
                             "num_key_value_heads": 8, "hidden_size": 2048}
            return original(method, path, body)

        fake.route = route
        self.env["SB_AI_FIT_HF_BASE"] = fake.url
        run = self.sb("fit", "org/dense", "--store", TARGET, "--bandwidth", "100")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(fake.doc["ceiling"]["bytes_per_token"], 2_000_000_000)
        self.assertEqual(fake.doc["ceiling"]["decode_tps"], 50.0)
        self.assertIn("decode up to 50 tok/s", run.stdout)


def load_tuner_module():
    import sys
    if str(CLI) not in sys.path:
        sys.path.insert(0, str(CLI))
    import ai_tune
    return ai_tune


class RestoreUnitTests(unittest.TestCase):
    """`restore_best` and its Ctrl-C handling, without a gateway."""

    def tuner(self, ai_tune, interrupts):
        tuner = ai_tune.Tuner.__new__(ai_tune.Tuner)
        tuner.best = {"load": {"ctx_size": 4096}}
        tuner.applied_doc = None
        tuner.run_id, tuner.target = "tune_x", TARGET
        tuner.err = open(os.devnull, "w")
        self.addCleanup(tuner.err.close)
        tuner.writes = []

        def put_doc(doc, note):
            tuner.applied_doc = None
            if len(tuner.writes) < interrupts:
                tuner.writes.append(("cut", note))
                raise KeyboardInterrupt()
            tuner.writes.append(("ok", note))
            tuner.applied_doc = json.loads(json.dumps(doc))
        tuner.put_doc = put_doc
        return tuner

    def test_unknown_applied_settings_always_restore(self):
        """The real `put_doc`, cut off by a Ctrl-C while the gateway still
        applies a trial, must leave the applied settings unknown, so
        `restore_best` writes the best settings back."""
        ai_tune = load_tuner_module()
        tuner = ai_tune.Tuner.__new__(ai_tune.Tuner)
        tuner.best = {"load": {"ctx_size": 4096}}
        # The engine runs the best settings, and the run knows it.
        tuner.applied_doc = json.loads(json.dumps(tuner.best))
        tuner.run_id, tuner.target, tuner.revision = "tune_x", TARGET, 3
        tuner.fail_types = (RuntimeError,)
        tuner.clock, tuner.apply_secs = time.monotonic, []
        puts = []

        class Gateway:
            def call(self, method, path, body=None, timeout=None, headers=None):
                puts.append(body["note"])
                if body["note"].endswith("trial"):
                    # The gateway applies the trial; the answer never arrives.
                    raise KeyboardInterrupt()
                return {"revision": 5, "apply": {"ok": True}}

        tuner.gw = Gateway()
        with self.assertRaises(KeyboardInterrupt):
            tuner.put_doc({"load": {"ctx_size": 4096, "flash_attn": "on"}}, "tune tune_x trial")
        self.assertIsNone(tuner.applied_doc)
        tuner.restore_best("control")
        self.assertEqual(puts, ["tune tune_x trial", "tune tune_x control"])
        self.assertEqual(tuner.applied_doc, tuner.best)
        # Known and equal: nothing to write.
        tuner.restore_best("control")
        self.assertEqual(len(puts), 2)

    def test_ctrl_c_inside_the_final_restore_restores_again(self):
        ai_tune = load_tuner_module()
        tuner = self.tuner(ai_tune, 1)
        tuner.restore_best_finally("restore")
        self.assertEqual([kind for kind, _ in tuner.writes], ["cut", "ok"])
        self.assertEqual(tuner.applied_doc, tuner.best)
        # Interrupted twice: a TuneError that names the undo, never a bare
        # KeyboardInterrupt that skips releasing the lock and hold.
        tuner = self.tuner(ai_tune, 2)
        with self.assertRaises(ai_tune.TuneError) as caught:
            tuner.restore_best_finally("restore")
        self.assertIn("sb ai revert", caught.exception.message)



class Clock:
    """A clock a test moves by hand (the budget's clock)."""

    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


def sim_tuner(seed, sd=0.07, effect=1.0, levers=("flash_attn",), concurrency="1", run_s=0.0,
              apply_s=0.0, budget="10h", clock=None, measure=None):
    """A tuner on a simulated machine, in this process: each run's decode is
    100 tok/s times the trial's true effect (`effect` when flash_attn is
    on) times a seeded Gaussian draw with SD `sd`; time to first token is
    steady (1%). Runs and applies move `clock` by `run_s` and `apply_s`.
    Talking to a gateway is stubbed out."""
    ai_tune = load_tuner_module()
    args = argparse.Namespace(samples=6, concurrency=concurrency, budget=budget, workload="chat",
                              poll_secs=None, json=True, levers=",".join(levers), wake=False,
                              dry_run=False)
    tuner = ai_tune.Tuner(None, args, (RuntimeError,), out=io.StringIO(), err=io.StringIO())
    rng = random.Random(seed)
    clock = clock or Clock()
    tuner.clock, tuner.started = clock, clock()
    tuner.run_id, tuner.target, tuner.model, tuner.lane = "tune_sim", TARGET, "m", "box"
    tuner.build, tuner.model_file, tuner.listing_row = "b1", "f1", {}
    tuner.kind, tuner.locked = "llama_cpp_router", set()
    tuner.best, tuner.applied_doc = {"load": {}}, {"load": {}}
    tuner.rows, tuner.logs = [], []
    tuner.record = tuner.rows.append
    tuner.log = tuner.logs.append
    tuner.gate = lambda kind, doc: None
    tuner.heartbeat = lambda force=False: None
    tuner.history = lambda: []
    tuner.ceiling = lambda: {}
    tuner.run_parallel = lambda jobs: [{"text": PRIMES}]
    tuner.state = {}

    def run(metrics):
        if measure:
            return measure(tuner.state, rng, metrics)
        factor = effect if tuner.state.get("flash_attn") == "on" else 1.0
        return {"decode": 100.0 * factor * rng.gauss(1, sd), "ttft_ms": 200.0 * rng.gauss(1, 0.01)}

    def fake_measure(*metrics):
        clock.now += run_s
        tuner.run_secs.setdefault(ai_tune.run_kind(metrics), []).append(run_s)
        return run(metrics), None

    def put_doc(doc, note):
        clock.now += apply_s
        tuner.apply_secs.append(apply_s)
        tuner.state = dict(doc.get("load") or {})
        tuner.applied_doc = json.loads(json.dumps(doc))
    tuner.measure = fake_measure
    tuner.put_doc = put_doc
    tuner.plan = tuner.levers()
    return ai_tune, tuner


class KeepRuleUnitTests(unittest.TestCase):
    """The keep rule: the noise is the SD of one run; a change runs control,
    candidate, control, ... until its gain over the controls' mean clearly
    clears 1.7 SD (kept) or clearly cannot (rejected); other metrics are
    held to the controls' mean by their own noise."""

    def test_noise_is_the_sd_of_one_run(self):
        ai_tune = load_tuner_module()
        runs = [{"decode": 100.0, "ttft_ms": 200.0}, {"decode": 110.0, "ttft_ms": 200.0},
                {"decode": 90.0, "ttft_ms": 202.0}]
        out = ai_tune.noise_sds(runs)
        self.assertAlmostEqual(out["decode"], 0.10)
        self.assertAlmostEqual(out["ttft_ms"], statistics.stdev([200.0, 200.0, 202.0]) / (602 / 3))
        self.assertEqual(ai_tune.noise_sds(runs[:1]), {})

    def judge(self, tuner, noise, controls, cands, last=False, metric="decode"):
        tuner.noise_data = [dict(decode=v, ttft_ms=200.0) for v in noise]
        tuner.noise_runs = len(noise)
        tuner.noise = load_tuner_module().noise_sds(tuner.noise_data)
        runs = lambda values: [dict(decode=v, ttft_ms=200.0) for v in values]  # noqa: E731
        return tuner.judge(metric, runs(controls), runs(cands), last)

    def test_a_clear_win_on_a_quiet_machine_is_kept_after_one_run(self):
        _, tuner = sim_tuner(1)
        keep, gain, reason, bar = self.judge(tuner, [100, 101, 100.5], [100, 100.5], [112])
        self.assertTrue(keep, reason)
        self.assertAlmostEqual(gain, 112 / 100.25 - 1)
        # Under the 2% floor: the bar is 1.7 x the floor, widened for so few runs.
        self.assertLess(bar, 0.06)
        # +3% is not clear after one run: one more run is asked for.
        keep, _, reason, _ = self.judge(tuner, [100, 101, 100.5], [100, 100.5], [103.3])
        self.assertIsNone(keep, reason)
        # -10%: rejected at once.
        keep, _, reason, _ = self.judge(tuner, [100, 101, 100.5], [100, 100.5], [90])
        self.assertFalse(keep)
        self.assertIn("inside the noise", reason)

    def test_a_win_on_a_noisy_machine_must_hold_over_two_runs(self):
        _, tuner = sim_tuner(1)
        noise = [100, 108, 93, 106, 94]  # SD about 6.5%
        keep, _, reason, _ = self.judge(tuner, noise, [100, 101], [160])
        self.assertIsNone(keep, reason)
        keep, _, reason, _ = self.judge(tuner, noise, [100, 101, 99], [160, 158])
        self.assertTrue(keep, reason)

    def test_the_last_run_decides(self):
        _, tuner = sim_tuner(1)
        noise = [100, 108, 93, 106, 94]
        # +9.5% after two runs: at the last run it must clear the bar (about
        # +11% here) and two standard errors, and it does not.
        keep, _, reason, _ = self.judge(tuner, noise, [100, 101, 99], [109, 110], last=True)
        self.assertFalse(keep, reason)
        self.assertIn("not clearly over the noise", reason)

    def test_a_side_metric_is_held_to_its_own_noise(self):
        _, tuner = sim_tuner(1)
        tuner.noise_data = [{"decode": 100.0, "ttft_ms": t} for t in (200.0, 216.0, 186.0)]
        tuner.noise_runs = 3
        controls = [{"decode": 100.0, "ttft_ms": 200.0}, {"decode": 100.5, "ttft_ms": 204.0}]
        # Time to first token 6% later: inside its own noise (SD about 7%).
        keep, _, reason, _ = tuner.judge("decode", controls, [{"decode": 120.0, "ttft_ms": 214.0}], False)
        self.assertTrue(keep, reason)
        # 60% later: rejected, whatever the decode win.
        keep, _, reason, _ = tuner.judge("decode", controls, [{"decode": 120.0, "ttft_ms": 324.0}], False)
        self.assertFalse(keep)
        self.assertIn("ttft_ms got", reason)

    def test_a_trial_that_does_not_load_is_failed_not_rejected(self):
        ai_tune, tuner = sim_tuner(1)

        def trial(entry, change, gate=True):
            raise ai_tune.ApplyFailed("the engine answered 400: model is already running")
        tuner.trial = trial
        tuner.last_control = {"prefill": 1000.0, "decode": 105.0}
        tuner.current_metric = "prefill"
        accepted, gain, reason = tuner.experiment({"lever": "ubatch_size", "metric": "prefill"},
                                                  {"ubatch_size": 1024})
        self.assertIsNone(accepted)
        self.assertEqual(gain, 0.0)
        self.assertIn("model is already running", reason)
        self.assertEqual([(r["role"], r.get("verdict")) for r in tuner.rows], [("verdict", "failed")])

    def keep_rate(self, sd, effect, runs=200):
        kept = 0
        for seed in range(runs):
            _, tuner = sim_tuner(seed, sd=sd, effect=effect)
            tuner.measure_noise("decode")
            accepted, _, _ = tuner.experiment(tuner.plan[0], {"flash_attn": "on"})
            kept += bool(accepted)
        return kept / runs

    def test_simulated_noisy_machine_keeps_real_wins_and_drops_small_ones(self):
        """200 seeded runs on a machine whose runs have an SD of 7% (the
        first real box): a real +20% is kept in at least 90% of them, a +5%
        (inside the noise) in at most 5%. The old range-based band kept the
        +20% in about 16%."""
        win = self.keep_rate(0.07, 1.20)
        fake = self.keep_rate(0.07, 1.05)
        print(f"\nsimulated SD 7%: +20% kept {win:.1%}, +5% kept {fake:.1%}", file=sys.stderr)
        self.assertGreaterEqual(win, 0.90)
        self.assertLessEqual(fake, 0.05)

    def test_a_weighted_lever_is_kept_while_aggregate_moves_inside_its_own_noise(self):
        """draft_max is judged on the workload-weighted speed. The noise runs
        measure aggregate and weighted too, so aggregate (the c2 speed, SD
        6% here) is held to its own noise, not to the floor."""
        def measure(state, rng, metrics):
            better = state.get("draft_max") == 3
            out = {"decode": 100.0 * rng.gauss(1, 0.01), "ttft_ms": 200.0 * rng.gauss(1, 0.01)}
            if "aggregate" in metrics or "weighted" in metrics:
                c1 = 100.0 * (2.0 if better else 1.0) * rng.gauss(1, 0.01)
                c2 = 180.0 * rng.gauss(1, 0.06)
                out.update(per_concurrency={1: c1, 2: c2}, aggregate=c2, weighted=(c1 + c2) / 2)
            return out
        _, tuner = sim_tuner(3, levers=("draft_max",), concurrency="1,2", measure=measure)
        tuner.listing_row = {"speculative": {"spec_type": "draft-mtp"}}
        tuner.best = {"load": {"spec_type": "draft-mtp", "draft_max": 1}}
        stop = tuner.tune()
        self.assertIn("aggregate", tuner.noise, tuner.noise)
        self.assertIn("weighted", tuner.noise, tuner.noise)
        self.assertGreater(tuner.noise["aggregate"], 0.02, tuner.noise)
        self.assertEqual([g[0] for g in tuner.gains], ["draft_max"], (stop, tuner.rows[-1]))
        self.assertEqual(tuner.best["load"]["draft_max"], 3)

    def test_a_ten_minute_budget_ends_within_a_minute_of_it(self):
        """Runs of 40 s and applies of 20 s on a noisy machine (many runs per
        change): the run ends within about a minute of its 10 minutes, the
        final restore included."""
        clock = Clock()
        _, tuner = sim_tuner(5, sd=0.07, effect=1.0, run_s=40.0, apply_s=20.0, budget="10m",
                             clock=clock, levers=("flash_attn", "ubatch_size", "cache_type"))
        tuner.take_hold_and_lock = lambda: setattr(tuner, "lock_id", "lock")
        tuner.release = lambda: setattr(tuner, "lock_id", None)
        code = tuner.run_trapped()
        self.assertEqual(code, 0)
        self.assertLessEqual(clock.now, 600 + 60, tuner.logs)
        result = json.loads(tuner.out.getvalue())
        self.assertIn("budget ran out", result["stopped"])
        # Out of budget between noise runs: untuned, nothing changed.
        clock = Clock()
        _, tuner = sim_tuner(5, sd=0.07, run_s=40.0, apply_s=20.0, budget="70s", clock=clock)
        tuner.take_hold_and_lock = lambda: setattr(tuner, "lock_id", "lock")
        tuner.release = lambda: setattr(tuner, "lock_id", None)
        self.assertEqual(tuner.run_trapped(), 0)
        result = json.loads(tuner.out.getvalue())
        self.assertFalse(result["tuned"])
        self.assertIn("budget ran out while measuring the noise", result["reason"])
        self.assertLessEqual(clock.now, 70 + 40)

    def test_the_printed_bar_is_the_judges_bar(self):
        """The bar the run prints after the noise runs, the `--json` bar
        and the judge's bar over the same runs are one number: the SD
        floored and widened for so few runs. The win-twice rule and the
        budget's round count use that SD too."""
        decode = iter([100.0, 104.5, 95.5, 103.0, 97.0])  # SD 3.8%, widened 5.2%

        def measure(state, rng, metrics):
            return {"decode": next(decode), "ttft_ms": 200.0}
        _, tuner = sim_tuner(4, measure=measure)
        tuner.measure_noise("decode")
        self.assertEqual(tuner.noise_runs, 5)
        printed = [line for line in tuner.logs if "a win must clear about" in line][-1]
        _, _, _, bar = tuner.judge("decode", list(tuner.noise_data), [dict(tuner.noise_data[0])],
                                   False)
        self.assertEqual(round(bar * 100, 1), tuner.noise_view()["win_needs_pct"])
        self.assertIn(f"about +{bar * 100:.0f}%", printed)
        # Raw SD under 5%, widened over it: a win must hold twice, and the
        # budget counts two rounds.
        self.assertLess(tuner.noise["decode"], 0.05)
        self.assertGreater(tuner.noise_sd("decode"), 0.05)
        keep, _, _, _ = tuner.judge("decode", list(tuner.noise_data),
                                    [{"decode": 200.0, "ttft_ms": 200.0}], False)
        self.assertIsNone(keep)
        tuner.apply_secs, tuner.run_secs = [10.0], {"decode": [50.0]}
        tuner.last_control = dict(tuner.noise_data[-1])
        tuner.gate_secs = {"greedy": [0.0]}
        tuner.budget, tuner.started = 10_000, 0.0
        tuner.clock = lambda: 10_000 - 200.0
        self.assertFalse(tuner.experiment_fits(tuner.plan[0]))  # two rounds: 240 s
        tuner.clock = lambda: 10_000 - 240.0
        self.assertTrue(tuner.experiment_fits(tuner.plan[0]))


def ai_tune_win_sd():
    return load_tuner_module().WIN_SD


def load_lever_tuner():
    """A simulated tuner whose plan is the mmap lever (judged on load time)."""
    return sim_tuner(7, levers=("mmap",))


def load_lever_run(budget, noise_s=40.0, load_run_s=240.0, apply_s=70.0, seed=7):
    """The mmap lever (judged on load time) on a fake engine: a run of the
    noise runs takes `noise_s`, a load-time run (three unloads and loads)
    `load_run_s`, an apply `apply_s`. Returns (tuner, clock)."""
    clock = Clock()

    def measure(state, rng, metrics):
        out = {"decode": 100.0 * rng.gauss(1, 0.03), "ttft_ms": 200.0 * rng.gauss(1, 0.01)}
        if "load_s" in metrics:
            out["load_s"] = 20.0 * rng.gauss(1, 0.03)
        return out
    ai_tune, tuner = sim_tuner(seed, levers=("mmap",), apply_s=apply_s, budget=budget,
                               clock=clock, measure=measure)

    def fake_measure(*metrics):
        took = load_run_s if "load_s" in metrics else noise_s
        clock.now += took
        tuner.run_secs.setdefault(ai_tune.run_kind(metrics), []).append(took)
        return measure(tuner.state, random.Random(len(tuner.rows)), metrics), None
    tuner.measure = fake_measure
    tuner.take_hold_and_lock = lambda: setattr(tuner, "lock_id", "lock")
    tuner.release = lambda: setattr(tuner, "lock_id", None)
    tuner.run_trapped()
    return tuner, clock


class BudgetUnitTests(unittest.TestCase):
    """What a lever costs is known before it starts, so the budget holds."""

    def test_a_load_time_lever_overruns_the_budget_by_at_most_one_run(self):
        """Noise runs take 40 s, a load-time run 240 s: before any load-time
        run was measured, its cost is taken from the apply (three loads),
        not from the 40 s noise runs. The run ends at most one load-time run
        past its budget."""
        for budget in (600, 900, 1200):
            tuner, clock = load_lever_run(f"{budget}s")
            overrun = clock.now - budget
            print(f"\nbudget {budget} s: ended at {clock.now:.0f} s (overrun {overrun:+.0f} s)",
                  file=sys.stderr)
            self.assertLessEqual(overrun, 240, (budget, clock.now, tuner.notes, tuner.logs))

    def test_a_load_time_run_not_measured_yet_costs_three_reloads(self):
        """Before any load-time run, one is priced at three applies (each a
        reload), never at one 40 s noise run; and a lever whose runs that
        price does not fit is not started, not even its first control."""
        ai_tune, tuner = load_lever_tuner()
        tuner.run_secs = {ai_tune.run_kind(tuner.noise_metrics): [40.0]}
        tuner.apply_secs = [70.0]
        self.assertEqual(tuner.run_cost("load_s"), 210.0)
        self.assertEqual(tuner.run_cost("decode"), 40.0)
        tuner.run_secs["load_s"] = [240.0]
        self.assertEqual(tuner.run_cost("load_s"), 240.0)
        del tuner.run_secs["load_s"]
        # Left: 500 s. At the noise run's price the change fits (a control
        # and one round: 40 + 2 x (70 + 40) = 260 s); at three reloads it
        # does not (210 + 2 x (70 + 210) = 770 s).
        tuner.budget, tuner.started, tuner.clock = 1000, 0.0, lambda: 500.0
        tuner.noise = {"decode": 0.02}
        self.assertFalse(tuner.experiment_fits(tuner.plan[0]))
        measured = []
        tuner.measure = lambda *metrics: measured.append(metrics) or ({}, None)
        self.assertFalse(tuner.starts_within_budget(tuner.plan[0]))
        self.assertEqual(measured, [])

    def test_the_first_trials_gate_is_counted(self):
        """A greedy gate that takes 100 s: a change whose rounds fit only
        without it is not started."""
        ai_tune, tuner = sim_tuner(1, run_s=40.0, apply_s=20.0, budget="1h")
        tuner.noise = {"decode": 0.03}
        tuner.noise_data = [{"decode": v, "ttft_ms": 200.0} for v in (100.0, 103.0, 97.0)]
        tuner.noise_runs = 3
        tuner.apply_secs, tuner.run_secs = [20.0], {"decode": [40.0]}
        tuner.last_control = {"decode": 100.0, "ttft_ms": 200.0}
        entry = tuner.plan[0]
        rounds = 2 * (20.0 + 40.0)  # one round on a quiet machine
        tuner.clock.now = 3600 - rounds - 50
        self.assertTrue(tuner.experiment_fits(entry))
        tuner.gate_secs = {"greedy": [100.0]}
        self.assertFalse(tuner.experiment_fits(entry))


if __name__ == "__main__":
    unittest.main()
