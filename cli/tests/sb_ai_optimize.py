#!/usr/bin/env python3
"""sb ai doctor, sb ai fit --store, the new `sb ai set` keys and sb ai tune,
against a fake gateway on a local port.

The fake's engine is fast and deterministic enough to tune: its decode and
prompt speed follow the load settings it was loaded with (each setting has
an effect factor), with a seeded jitter of 5 to 10%; it keeps the slot count
(concurrent requests beyond it wait), answers needles unless a setting
breaks them, changes its greedy answer when a setting is lossy, and fails
loads it is told to fail. Every run goes through the real `sb` dispatcher
in a child process."""

import json
import os
import random
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
        self.base_decode = 800.0      # tok/s; a 64-token answer takes ~80 ms
        self.base_ttft_ms = 200.0     # long enough that scheduling noise stays under 1%
        self.base_prefill = 60000.0   # tok/s
        self.effects = {}             # (key, json value) -> {"decode": f, "ttft": f, "prefill": f}
        self.repeat_effects = {}      # (key, json value) -> [decode factor per apply]
        self.apply_factor = 1.0
        self.loaded = {}
        self.needle_breaks = set()    # (key, json value) that lose needles
        self.greedy_breaks = set()    # (key, json value) that change the greedy answer
        self.load_fails = set()       # (key, json value) that fail to load
        self.soak_breaks = False      # concurrent long requests fail at parallel > 1
        self.slots = threading.Semaphore(1)
        self.slot_count = 1
        self.requests = []            # (kind, completed)
        self.applies = []             # the load of every apply

    def factor(self, metric):
        out = self.apply_factor if metric == "decode" else 1.0
        for key, value in self.loaded.items():
            effect = self.effects.get((key, json.dumps(value)))
            if effect:
                out *= effect.get(metric, 1.0)
        return out

    def jitter(self, spread=0.05):
        """Seeded run-to-run noise: 5% on the tuned metrics (decode, prefill),
        1% on time to first token, so the 3% rule on a metric a lever does
        not touch is tested by its own case, not by chance."""
        with self.rng_lock:
            return 1.0 + self.rng.uniform(-spread, spread)

    def apply(self, load):
        for key, value in load.items():
            if (key, json.dumps(value)) in self.load_fails:
                return f"could not load with {key}={value}"
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
                 "queue_depth": 0, "in_flight": self.lane_in_flight}]}
        if method == "POST" and path == "/v1/hosts/gpu-box/holds":
            hold_id = f"hold-{len(self.holds) + 1}"
            self.holds[hold_id] = body
            if body.get("wake"):
                self.lane_state = "healthy"
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
        # Eight samples a run: the 3% rule on the other metrics must not trip
        # on the fake's own jitter.
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
        self.assertIn("did not beat both controls", verdict["reason"])
        self.assert_released()

    def test_a_win_that_costs_another_metric_more_than_three_percent_is_rejected(self):
        # Decode +15%, but the first token comes 30% later.
        self.effect("flash_attn", "on", decode=1.15, ttft=0.70)
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "rejected", verdict)
        self.assertIn("ttft_ms got", verdict["reason"])
        self.assertIn("more than 3%", verdict["reason"])
        self.assertNotIn("flash_attn", self.fake.doc.get("load") or {})

    def test_a_surprising_win_is_repeated_and_a_fluke_is_dropped(self):
        # +60% on the first apply only: the repeat finds nothing.
        self.fake.engine.repeat_effects[("flash_attn", json.dumps("on"))] = [1.6, 1.0, 1.0]
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        candidates = [row for row in self.fake.ledger if row.get("role") == "candidate"]
        self.assertEqual(len(candidates), 2, "the surprising win was not repeated")
        verdict = self.verdicts()[-1]
        self.assertEqual(verdict["verdict"], "rejected", verdict)
        self.assertIn("did not repeat", verdict["reason"])
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
        # Twelve samples: the 3% rule on time to first token, a metric this
        # lever does not touch, must not trip on the fake's jitter.
        run = self.tune("--levers", "draft_max", "--concurrency", "1,2", "--samples", "12")
        self.assertEqual(run.returncode, 0, run.stderr)
        result = json.loads(run.stdout)
        table = result["sweeps"]["draft_max"]
        self.assertEqual(len(table), 4, table)
        self.assertTrue(all(set(per) == {"1", "2"} for per in table.values()), table)
        self.assertEqual(self.fake.doc["load"]["draft_max"], 3, self.verdicts())

    def test_a_failed_load_is_a_rejection_not_a_crash(self):
        self.fake.engine.load_fails.add(("flash_attn", json.dumps("on")))
        run = self.tune("--levers", "flash_attn")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("did not load", self.verdicts()[-1]["reason"])
        self.assert_released()

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
        self.assert_released()

    def test_a_busy_lane_is_refused(self):
        self.fake.lock_refusal = ("lane_busy", "lane `box` has 1 request(s) in flight or queued")
        run = self.sb("tune", TARGET, "--levers", "flash_attn")
        self.assertEqual(run.returncode, 1)
        self.assertIn("in flight", run.stderr)
        self.assertEqual(self.fake.calls("PUT"), [])
        self.assertEqual(self.fake.holds, {}, "the hold outlived the refusal")

    def test_a_quiet_window_is_refused(self):
        self.fake.lock_refusal = ("quiet_window", "inside the quiet window 00:30-03:00 (UTC)")
        run = self.sb("tune", TARGET)
        self.assertEqual(run.returncode, 1)
        self.assertIn("quiet window", run.stderr)
        self.assertEqual(self.fake.calls("POST", "/v1/chat/completions"), [])

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
        self.assertEqual(self.verdicts()[-1]["verdict"], "accepted")

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


if __name__ == "__main__":
    unittest.main()
