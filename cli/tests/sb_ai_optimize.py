#!/usr/bin/env python3
"""sb ai doctor, sb ai fit --store and the new `sb ai set` keys,
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
        self.base_decode = 1600.0     # tok/s; a 64-token answer takes ~40 ms
        self.base_ttft_ms = 45.0
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

    def jitter(self):
        with self.rng_lock:
            return 1.0 + self.rng.uniform(-0.06, 0.06)

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
        self.sidecars = ["mtp"]
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
        if apply == "now":
            problem = self.engine.apply(body.get("load") or {})
            view["apply"] = ({"ok": False, "error": problem, "message": problem} if problem
                             else {"ok": True, "message": "reloaded with the new settings"})
        return 200, view

    def chat(self, handler, body):
        engine = self.engine
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
                    + prompt_tokens / (engine.base_prefill * engine.factor("prefill")) * 1000) * engine.jitter()
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
            return 200, {"object": "list", "data": [
                {"lane": "box", "build": self.build, "models": [
                    {"id": "m", "file": self.model_file, "sidecars": self.sidecars}]}]}
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
                    body = dict(body, revision=int(self.headers["If-Match"]))
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
