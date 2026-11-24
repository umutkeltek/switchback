#!/usr/bin/env python3
"""sb ai check, set --check and the pending checks of load, against a fake
gateway on a local port.

The fake engine behaves like a llama.cpp router's one model where it
matters here: it splits the set context over its slots unless kv-unified is
on (a prompt longer than one request's share is refused with 400), refuses
every request in flight when the shared pool overflows, can crash on a long
request (500, then a new process on a new port), can answer from a cache
(instantly, every prompt token cached), can run out of room while thinking
(finish_reason length, nothing written), and reads the number in a picture
from the PNG itself (decoding the check's own pixel font), misreading the
last digit on chosen settings. Every run goes through the real `sb`
dispatcher in a child process."""

import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
sys.path.insert(0, str(CLI))
import ai_check  # noqa: E402

KEY = "sk-test-5d1e0c7a"
TARGET = "box/m"
CHARS_PER_TOKEN = 5


TEMPLATES = {}


def decode_picture(data):
    """The number and shape a check picture shows, read from its pixels."""
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    pos, idat, width, height = 8, b"", 0, 0
    while pos < len(data):
        length = int.from_bytes(data[pos:pos + 4], "big")
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            width, height = int.from_bytes(body[:4], "big"), int.from_bytes(body[4:8], "big")
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    raw = zlib.decompress(idat)
    stride = width * 3 + 1

    def px(x, y):
        i = y * stride + 1 + x * 3
        return tuple(raw[i:i + 3])

    # Each digit slot against every digit's own pen strokes: the best match.
    number = ""
    for n in range(3):
        x0, y0 = ai_check.digit_origin(n)
        slot = {(x, y) for y in range(y0 - 15, y0 + ai_check.DIGIT_HEIGHT + 15)
                for x in range(x0 - 15, x0 + ai_check.DIGIT_WIDTH + 15) if px(x, y) == (0, 0, 0)}
        best, score = None, -1.0
        for digit in "0123456789":
            covered = TEMPLATES.setdefault((digit, n), ai_check.digit_pixels(digit, x0, y0))
            match = len(covered & slot) / len(covered | slot)
            if match > score:
                best, score = digit, match
        number += best
    colour = {v: k for k, v in ai_check.COLOURS.items()}.get(px(520, 180))
    if px(520 - 75, 180 - 75) != (255, 255, 255):
        shape = "square"
    elif px(520 - 60, 180 + 60) != (255, 255, 255):
        shape = "triangle"
    else:
        shape = "circle"
    return number, shape, colour


class Engine:
    """One model of a llama.cpp router, configured from the settings."""

    def __init__(self):
        self.lock = threading.Lock()
        self.base_args = {"ctx_size": 24576, "parallel": 3, "cache_type_k": "f16",
                          "cache_type_v": "f16"}
        self.projector = False
        self.port = 41000
        self.status = "loaded"
        self.failed = False
        self.unified = False
        self.cfg = dict(self.base_args)
        self.in_flight = 0       # tokens held in the pool by requests in flight
        self.flights = []        # events of the requests in flight (an overflow ends them all)
        self.crash_rule = None   # cfg, tokens -> True: the request crashes the process
        self.cache_answers = False
        self.capped = False
        self.misread = None      # cfg -> True: every 6th picture loses its last digit
        self.read_tps = 30000.0
        self.prompts = []
        self.pictures = 0
        # The engine's own memory measurement: a 4 GB model on a 24 GB card
        # (None: the engine measures nothing, as llama.cpp's router today).
        self.memory = {"weights": 4e9, "kv_per_token": 1000, "compute": 0.5e9, "card": 24e9}
        self.model_path = "/models/m.gguf"

    def configure(self, doc):
        load = doc.get("load") or {}
        args = doc.get("engine_args") or {}
        self.cfg = dict(self.base_args)
        for key in ("ctx_size", "parallel", "cache_type_k", "cache_type_v"):
            if key in load:
                self.cfg[key] = load[key]
        self.unified = str(args.get("kv-unified", "")).lower() in ("true", "on", "1")
        self.restart()

    def restart(self):
        self.port += 1
        self.status = "loaded"

    def per_request(self):
        return self.cfg["ctx_size"] if self.unified else self.cfg["ctx_size"] // self.cfg["parallel"]

    def args(self):
        out = ["/app/llama-server", "--port", str(self.port), "--ctx-size", str(self.cfg["ctx_size"]),
               "--parallel", str(self.cfg["parallel"]), "--cache-type-k", self.cfg["cache_type_k"],
               "--cache-type-v", self.cfg["cache_type_v"], "--flash-attn", "on", "--fit", "off",
               "--n-gpu-layers", "999"]
        if self.unified:
            out.append("--kv-unified")
        if self.projector:
            out += ["--mmproj", "/models/mmproj.gguf"]
        return out

    def measurement(self):
        """The engine's memory measurement at the running settings, as the
        gateway's listing summarises it (`total_bytes` only when the
        gateway passes the card's size on)."""
        if not self.memory or self.status not in ("loaded", "loading"):
            return None
        kv = int(self.memory["kv_per_token"] * self.cfg["ctx_size"])
        part = {"n_ctx": self.cfg["ctx_size"], "host_bytes": 0, "context_bytes": kv,
                "bytes": int(self.memory["weights"] + self.memory["compute"]) + kv}
        if self.memory.get("card"):
            part["total_bytes"] = int(self.memory["card"])
        return {"status": "measured", "effective": part}

    def row(self):
        row = {"id": "m", "target": TARGET, "file": "file-a", "status": self.status,
               "failed": self.failed, "args": self.args(), "memory": self.measurement()}
        if getattr(self, "no_file", False):
            # A router that lists no file: the model's argument names it.
            del row["file"]
            row["args"] += ["--model", self.model_path]
        return row


class Fake:
    def __init__(self):
        self.engine = Engine()
        self.lock = threading.Lock()
        self.seen = []
        self.doc = {}
        self.revision = 0
        self.history = []
        self.locks, self.holds, self.ledger = {}, {}, []
        self.props = False    # the gateway has GET .../engine-props
        self.effective = False  # the gateway's engine rows carry `effective`
        self.quiet = None       # a quiet window in force: lock renewals name it
        self.quiet_in = None    # seconds to the next quiet window: new locks end there
        self.lock_lost = False  # lock renewals answer 404
        self.lock_refused = None  # new locks are refused with this reason (409)
        self.load_fails = False  # loads answer 502
        self.others = []        # other models the engine lists
        self.machines = []      # the gateway's machine facts (GET /v1/engines `machines`)
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def write(self, doc, note=None, action="put"):
        self.doc = doc
        self.revision += 1
        self.history.append({"revision": self.revision, "action": action + (f": {note}" if note else ""),
                             "document": json.loads(json.dumps(doc)),
                             "at_ms": int(time.time() * 1000)})
        self.engine.configure(doc)

    def view(self):
        return {"target": TARGET, "revision": self.revision, "durable": True,
                "settings_hash": f"h{self.revision}", "effective": {}, "document": self.doc,
                "load": {"engine": {"lane": "box", "kind": "llama_cpp_router",
                                    "status": self.engine.status}, "settings": {}}}

    # ------------------------------------------------------------ chat

    def chat(self, body):
        engine = self.engine
        content = body["messages"][0]["content"]
        picture = None
        if isinstance(content, list):
            text = next(p["text"] for p in content if p.get("type") == "text")
            url = next(p["image_url"]["url"] for p in content if p.get("type") == "image_url")
            import base64
            picture = decode_picture(base64.b64decode(url.split(",", 1)[1]))
            content = text
        tokens = len(content) // CHARS_PER_TOKEN + 20
        with engine.lock:
            engine.prompts.append(content)
            if engine.status not in ("loaded", "sleeping"):
                engine.restart()  # the router loads the model again for the request
            if tokens > engine.per_request():
                return 400, {"error": {"message": f"the request exceeds the available context size "
                                                  f"({engine.per_request()} tokens), try increasing it",
                                       "type": "invalid_request_error"}}
            if engine.crash_rule and engine.crash_rule(engine.cfg, tokens):
                engine.status, engine.failed = "unloaded", True
                return 500, {"error": {"message": "CUDA error: out of memory", "type": "engine_error"}}
            held = tokens + int(body.get("max_tokens") or 0)
            engine.in_flight += held
            mine = threading.Event()
            engine.flights.append(mine)
            if engine.in_flight > engine.cfg["ctx_size"]:
                for event in engine.flights:
                    event.set()
        try:
            if engine.cache_answers:
                read = 0.001
            else:
                read = tokens / engine.read_tps
            if mine.wait(read):
                return 500, {"error": {"message": "Context size has been exceeded", "type": "engine_error"}}
        finally:
            with engine.lock:
                engine.in_flight -= held
                engine.flights.remove(mine)
        usage = {"prompt_tokens": tokens, "completion_tokens": 40, "total_tokens": tokens + 40}
        if engine.cache_answers:
            usage["prompt_tokens_details"] = {"cached_tokens": tokens}
        if engine.capped:
            message, finish = {"content": "", "reasoning_content": "thinking " * 300}, "length"
        elif picture is not None:
            number, shape, colour = picture
            with engine.lock:
                engine.pictures += 1
                count = engine.pictures
            if engine.misread and engine.misread(engine.cfg) and count % 6 == 0:
                number = number[:-1] + "0" if number[-1] != "0" else number[:-1] + "1"
            message, finish = {"content": f"The number is {number}, next to a {colour} {shape}."}, "stop"
        else:
            codes = re.findall(r"archive is (\d{4}-[A-Z]+[0-9A-F]{2})\.", content)
            names = ("Larkspur", "Wrenfield", "Ostmark")
            message = {"content": "\n".join(f"{n}: {c}" for n, c in zip(names, codes))}
            finish = "stop"
        return 200, {"id": "x", "object": "chat.completion", "model": TARGET,
                     "choices": [{"index": 0, "message": dict(message, role="assistant"),
                                  "finish_reason": finish}], "usage": usage}

    # ------------------------------------------------------------ routes

    def route(self, method, path, body):
        base = f"/v1/models/{TARGET}"
        if method == "GET" and path == base + "/settings":
            return 200, {"model": TARGET, "targets": [self.view()], "revision": self.revision}
        if method == "PUT" and path == base + "/settings":
            if body.get("revision") != self.revision:
                return 409, {"error": {"message": "stale", "type": "revision_conflict"}}
            # A store lock, as the gateway refuses it: 423 when a locked
            # load key would change.
            for locked in self.doc.get("locked") or []:
                key = locked[5:] if locked.startswith("load.") else None
                if key and (body.get("load") or {}).get(key) != (self.doc.get("load") or {}).get(key):
                    return 423, {"error": {"message": f"`{locked}` of {TARGET} is locked (gateway)",
                                           "type": "locked"}}
            doc = dict(body)
            apply = doc.pop("apply", None)
            note = doc.pop("note", None)
            doc.pop("revision", None)
            doc.pop("wake", None)
            self.write(doc, note)
            view = self.view()
            view["apply"] = {"ok": True, "message": "reloaded with the new settings" if apply == "now"
                             else "the engine re-reads the settings on the next load"}
            return 200, view
        if method == "GET" and path == base + "/settings/history":
            return 200, {"target": TARGET, "revision": self.revision, "history": self.history[::-1]}
        if method == "POST" and path == base + "/load":
            if self.load_fails:
                return 502, {"error": {"message": "the engine did not answer", "type": "engine_error"}}
            with self.engine.lock:
                if self.engine.status not in ("loaded", "sleeping"):
                    self.engine.restart()
            return 200, {"target": TARGET, "model": "m", "lane": "box", "status": "loaded"}
        if method == "POST" and path == base + "/unload":
            with self.engine.lock:
                self.engine.status = "unloaded"
            return 200, {"target": TARGET, "status": "unloaded", "message": "unloaded"}
        if method == "GET" and path == base + "/engine-props":
            if not self.props:
                return 404, {"error": {"message": "not found", "type": "not_found"}}
            return 200, {"target": TARGET, "n_ctx_per_slot": self.engine.per_request()
                         if not self.engine.unified else self.engine.cfg["ctx_size"],
                         "slots": self.engine.cfg["parallel"], "vision": self.engine.projector}
        if method == "GET" and path == "/v1/engines":
            row = self.engine.row()
            if self.effective:
                row["effective"] = ai_check.truth_from_args(row["args"])
            return 200, {"object": "list", "data": [{"lane": "box", "build": "b100",
                                                     "models": [row] + self.others}],
                         "machines": self.machines}
        if method == "GET" and path == "/v1/workloads/capacity":
            return 200, {"object": "list", "data": [{"name": "box", "host": "gpu-box", "state": "healthy",
                                                     "experiments": True}]}
        if method == "POST" and path == "/v1/hosts/gpu-box/holds":
            hold_id = f"hold-{len(self.holds) + 1}"
            self.holds[hold_id] = body
            return 201, {"id": hold_id}
        if method == "DELETE" and path.startswith("/v1/hosts/gpu-box/holds/"):
            self.holds.pop(path.rsplit("/", 1)[1], None)
            return 200, {"deleted": True}
        if method == "POST" and path == "/v1/optimize/locks":
            if self.lock_refused:
                return 409, {"error": {"message": f"refused: {self.lock_refused}", "type": "conflict",
                                       "reason": self.lock_refused}}
            lock_id = f"lock-{len(self.locks) + 1}"
            self.locks[lock_id] = body
            now = int(time.time() * 1000)
            expires = now + int(body.get("ttl_secs") or 1800) * 1000
            if self.quiet_in is not None:
                expires = min(expires, now + int(self.quiet_in * 1000))
            return 201, {"id": lock_id, "created_ms": now, "expires_at_ms": expires}
        if method == "PATCH" and path.startswith("/v1/optimize/locks/"):
            if self.lock_lost:  # it ran out, or the gateway restarted
                self.locks.pop(path.rsplit("/", 1)[1], None)
                return 404, {"error": {"message": "no tuning lock", "type": "not_found"}}
            return 200, {"id": path.rsplit("/", 1)[1], "quiet_window": self.quiet}
        if method == "DELETE" and path.startswith("/v1/optimize/locks/"):
            self.locks.pop(path.rsplit("/", 1)[1], None)
            return 200, {"deleted": True}
        if method == "POST" and path == "/v1/optimize/quality":
            problem = quality_problem(body)
            if problem:
                return 400, {"error": {"message": problem, "type": "text_not_allowed"}}
            row = dict(body, id=len(self.ledger) + 1, at_ms=int(time.time() * 1000) + len(self.ledger))
            self.ledger.append(row)
            return 201, {"id": row["id"], "durable": True}
        if method == "GET" and path.startswith("/v1/optimize/quality"):
            rows = [r for r in self.ledger if "suite=" not in path or
                    f"suite={r['suite']}" in path]
            return 200, {"object": "list", "data": rows[::-1]}
        if method == "GET" and path.startswith("/v1/optimize/report"):
            return 200, {"object": "optimize.report", "targets": [{"target": TARGET}], "findings": [],
                         "hosts": []}
        return 404, {"error": {"message": f"no route {method} {path}", "type": "not_found"}}

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
                if self.command == "PUT" and "revision" not in body and self.headers.get("If-Match"):
                    body = dict(body, revision=int(self.headers["If-Match"].strip('"')))
                if self.command == "POST" and self.path == "/v1/chat/completions":
                    status, payload = fake.chat(body)
                    if status >= 400 and getattr(fake, "bare_errors", False):
                        # The gateway passes no engine text on.
                        payload = {"error": {"message": f"upstream {status} error", "type": "upstream"}}
                else:
                    status, payload = fake.route(self.command, self.path, body)
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = serve

        return Handler

    def chats(self):
        with self.lock:
            return [(b, h) for m, p, b, h in self.seen if p == "/v1/chat/completions"]


FORBIDDEN = ("prompt", "response", "text", "content", "message", "answer", "quote", "raw", "body",
             "secret", "token", "key")
STRINGS = ("run_id", "target", "suite", "suite_version", "model_file", "engine_build", "settings_hash",
           "status", "rescored_from", "stop_reason")


def quality_problem(row, path=""):
    """The gateway's quality-ledger rule: names only, strings only in the
    identity fields."""
    if isinstance(row, dict):
        for key, value in row.items():
            ids = path == "per_case"
            if not ids and (not re.fullmatch(r"[a-z][a-z0-9_]{0,31}", key) or any(
                    key.startswith(w) or w in key.split("_") for w in FORBIDDEN)):
                return f"bad key {key}"
            problem = quality_problem(value, key if not path else f"{path}.{key}")
            if problem:
                return problem
    elif isinstance(row, list):
        for item in row:
            problem = quality_problem(item, path)
            if problem:
                return problem
    elif isinstance(row, str) and path not in STRINGS:
        return f"string at {path}"
    return None


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-ai-check-")
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

    def check(self, *args):
        run = self.sb("check", TARGET, "--json", *args)
        self.assertIn(run.returncode, (0, 1), run.stderr)
        return run, json.loads(run.stdout)

    def by_check(self, report):
        return {v["check"]: v for v in report["checks"]}

    def released(self):
        self.assertEqual(self.fake.locks, {})
        self.assertEqual(self.fake.holds, {})


class TruthTests(unittest.TestCase):
    def test_the_truth_view_splits_the_set_context_over_the_slots(self):
        args = ["llama-server", "--ctx-size", "98304", "--parallel", "3", "--cache-type-k", "q8_0",
                "--cache-type-v", "q8_0", "--fit", "off", "--n-gpu-layers", "999", "--port", "54387"]
        truth = ai_check.truth_from_args(args)
        self.assertEqual((truth["ctx_size"], truth["slots"], truth["kv_unified"]), (98304, 3, False))
        self.assertEqual(truth["per_request_ctx"], 32768)
        self.assertEqual((truth["cache_type_k"], truth["cpu_weights"], truth["port"]), ("q8_0", "no", 54387))
        self.assertIn("one request can use 32,768", ai_check.split_finding(truth))
        shared = ai_check.truth_from_args(args + ["--kv-unified"])
        self.assertEqual(shared["per_request_ctx"], 98304)
        self.assertIsNone(ai_check.split_finding(shared))

    def test_weights_on_the_cpu_are_named(self):
        base = ["llama-server", "--ctx-size", "8192", "--parallel", "1"]
        self.assertEqual(ai_check.truth_from_args(base + ["--n-cpu-moe", "4"])["cpu_weights"], "yes")
        self.assertEqual(ai_check.truth_from_args(base + ["-ot", "blk.1.ffn=CPU"])["cpu_weights"], "yes")
        self.assertEqual(ai_check.truth_from_args(base + ["--fit", "on"])["cpu_weights"], "maybe")

    def test_a_picture_carries_its_number_and_shape(self):
        for digits in ("102", "345", "678", "981"):
            rng = ai_check.fresh_rng()
            rng.randrange = lambda a, b, d=iter([int(digits)]): next(d)
            data, number, _, _ = ai_check.picture(rng)
            self.assertEqual((number, decode_picture(data)[0]), (digits, digits))
        for _ in range(5):
            data, number, shape, colour = ai_check.picture(ai_check.fresh_rng())
            self.assertEqual(decode_picture(data), (number, shape, colour))

    def test_long_prompts_are_fresh_and_hold_three_codes_at_their_depths(self):
        a, codes = ai_check.long_prompt(ai_check.fresh_rng(), 4000, 5.0)
        b, _ = ai_check.long_prompt(ai_check.fresh_rng(), 4000, 5.0)
        self.assertNotEqual(a.splitlines()[0], b.splitlines()[0])
        self.assertEqual(len(set(codes)), 3)
        spots = [a.index(code) / len(a) for code in codes]
        for spot, depth in zip(spots, ai_check.DEPTHS):
            self.assertAlmostEqual(spot, depth, delta=0.05)

    def test_guarded_changes_name_memory_and_context_keys_only(self):
        before = {"load": {"ctx_size": 8192, "temp": 0.6}}
        after = {"load": {"ctx_size": 16384, "temp": 0.7}, "engine_args": {"kv-unified": True}}
        self.assertEqual(ai_check.guarded_changes(before, after), ["load.ctx_size", "engine_args.kv-unified"])
        self.assertEqual(ai_check.guarded_changes(before, {"load": {"ctx_size": 8192, "temp": 0.2}}), [])


class CheckTests(Base):
    def test_a_split_context_fails_long_and_the_truth_view_shows_one_requests_share(self):
        # A set context split over 3 slots (kv-unified off).
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}})
        run, report = self.check("--checks", "long")
        self.assertEqual(run.returncode, 1)
        self.assertEqual(report["truth"]["per_request_ctx"], 8192)
        long = self.by_check(report)["long"]
        self.assertFalse(long["passed"])
        self.assertEqual(long["result"], "context_exceeded")
        self.assertEqual(long["requests"][0]["status"], 400)
        row = self.fake.ledger[-1]
        self.assertEqual((row["suite"], row["passed"], row["stop_reason"], row["http_status"]),
                         ("check-long", 0, "context_exceeded", 400))
        self.assertEqual((row["settings_revision"], row["engine_build"], row["model_file"]), (1, "b100", "file-a"))
        self.assertEqual((row["set_ctx"], row["per_request_ctx"], row["kv_unified"]), (24576, 8192, 0))
        self.released()

    def test_the_same_context_shared_by_the_slots_passes_long_and_pool(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run, report = self.check("--checks", "long,pool")
        self.assertEqual(run.returncode, 0, run.stderr)
        checks = self.by_check(report)
        self.assertTrue(checks["long"]["passed"])
        self.assertTrue(checks["pool"]["passed"], checks["pool"].get("why"))
        # The pool check filled the pool: three requests at once, under its size.
        self.assertEqual(len(checks["pool"]["requests"]), 3)
        self.assertLessEqual(checks["pool"]["sent_tokens"], 24576)
        self.assertGreater(checks["pool"]["sent_tokens"], 0.9 * 24576)
        self.released()

    def test_a_crash_on_the_first_long_request_fails_pool(self):
        # A shared pool too big for the card: loads, then the first long request runs out of
        # GPU memory and the process ends (the router starts a new one).
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        self.fake.engine.crash_rule = lambda cfg, tokens: tokens > 12000
        run, report = self.check("--checks", "pool")
        self.assertEqual(run.returncode, 1)
        pool = self.by_check(report)["pool"]
        self.assertFalse(pool["passed"])
        self.assertIn(pool["result"], ("engine_crashed", "engine_restarted"))
        self.assertTrue(pool["engine"]["crashed"] or pool["engine"]["restarted"])
        self.assertIn("out of memory", pool["why"])
        self.assertEqual(self.fake.ledger[-1]["engine_crashed"] or self.fake.ledger[-1]["engine_restarted"], 1)

    def test_a_crash_on_the_first_short_request_fails_every_check(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        self.fake.engine.crash_rule = lambda cfg, tokens: True
        run, report = self.check("--checks", "long,pool")
        self.assertEqual(run.returncode, 1)
        for name in ("long", "pool"):
            verdict = self.by_check(report)[name]
            self.assertEqual((verdict["passed"], verdict["result"]), (False, "engine_crashed"), verdict)
            self.assertIn("first, short request", verdict["why"])
        self.assertEqual([r["engine_crashed"] for r in self.fake.ledger], [1, 1])
        # Only the first request was sent.
        self.assertEqual(len(self.fake.chats()), 1)

    def test_a_bare_400_over_one_requests_context_is_named_as_such(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}})
        self.fake.bare_errors = True
        run, report = self.check("--checks", "long")
        long = self.by_check(report)["long"]
        self.assertEqual(long["result"], "context_exceeded")
        self.assertIn("one request can use 8,192", long["requests"][0]["error"])

    def test_the_model_file_stays_the_same_across_loads_without_a_file_field(self):
        self.fake.engine.no_file = True
        self.fake.write({"load": {"ctx_size": 16384, "parallel": 1}})
        self.check("--checks", "long")
        self.fake.engine.restart()  # a new process on a new port
        self.check("--checks", "long")
        files = {r["model_file"] for r in self.fake.ledger}
        self.assertEqual(len(files), 1, files)
        self.assertNotIn("/models", next(iter(files)))

    def test_a_cached_answer_is_not_a_pass(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 1}})
        self.fake.engine.cache_answers = True
        run, report = self.check("--checks", "long")
        self.assertEqual(run.returncode, 1)
        long = self.by_check(report)["long"]
        self.assertEqual((long["passed"], long["result"]), (False, "not_read"))
        # Every check request asks for no cache and starts with its own line.
        chats = self.fake.chats()
        firsts = [b["messages"][0]["content"].splitlines()[0] for b, _ in chats]
        self.assertEqual(len(set(firsts)), len(firsts))
        for body, headers in chats:
            self.assertEqual(body["switchback"]["settings"]["extra"]["cache_prompt"], False)
            self.assertIn("x-switchback-tune-lock", headers)

    def test_a_gateway_with_the_truth_view_gets_no_cache_on_every_request(self):
        self.fake.effective = True
        self.fake.write({"load": {"ctx_size": 16384, "parallel": 1}})
        run, report = self.check("--checks", "long")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertTrue(all(b["switchback"].get("no_cache") is True for b, _ in self.fake.chats()))

    def test_thinking_cut_at_max_tokens_is_its_own_result_not_a_failure(self):
        self.fake.write({"load": {"ctx_size": 16384, "parallel": 1}})
        self.fake.engine.capped = True
        run, report = self.check("--checks", "long")
        self.assertEqual(run.returncode, 0, run.stderr)
        long = self.by_check(report)["long"]
        self.assertIsNone(long["passed"])
        self.assertEqual((long["result"], long["length_capped"]), ("length_capped", 1))
        row = self.fake.ledger[-1]
        self.assertEqual((row["status"], row["length_capped"], row["score"]), ("partial", 1, None))
        self.assertIsNone(report["passed"])

    def test_picture_counts_the_numbers_read_right(self):
        self.fake.engine.projector = True
        self.fake.write({"load": {"ctx_size": 16384, "parallel": 1}})
        self.fake.engine.misread = lambda cfg: True
        run, report = self.check("--checks", "picture", "--runs", "12")
        picture = self.by_check(report)["picture"]
        self.assertEqual((picture["number_right"], picture["runs"]), (10, 12))
        self.assertEqual(picture["shape_right"], 12)
        self.assertFalse(picture["passed"])  # 10 of 12 is under 90% with nothing to compare against
        self.assertEqual(self.fake.ledger[-1]["number_right"], 10)

    def test_a_model_without_a_projector_skips_picture(self):
        self.fake.write({"load": {"ctx_size": 16384, "parallel": 1}})
        run, report = self.check("--checks", "picture")
        self.assertEqual(self.by_check(report)["picture"]["result"], "skipped")
        self.assertEqual(self.fake.ledger, [])

    def test_the_engines_own_props_set_the_per_request_context(self):
        self.fake.props = True
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}})
        run, report = self.check("--checks", "long", "--per-request")
        self.assertEqual(report["truth"]["source"], "props")
        self.assertEqual(report["truth"]["per_request_ctx"], 8192)
        self.assertTrue(self.by_check(report)["long"]["passed"])


class GuardTests(Base):
    def test_set_check_keeps_a_change_that_passes(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}})
        run = self.sb("set", TARGET, "engine_args.kv-unified=true", "--load", "--check",
                      "--checks", "long,pool")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("kept", run.stdout)
        self.assertEqual(self.fake.doc["engine_args"], {"kv-unified": True})
        self.released()

    def test_set_check_puts_the_old_settings_back_when_long_fails(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        before = json.loads(json.dumps(self.fake.doc))
        run = self.sb("set", TARGET, "engine_args.kv-unified=false", "--load", "--check",
                      "--checks", "long")
        self.assertEqual(run.returncode, 1)
        self.assertEqual(self.fake.doc, before)
        self.assertEqual(self.fake.revision, 3)
        self.assertIn("long failed (context_exceeded); back to revision 1", self.fake.history[-1]["action"])
        self.assertIn("put back the settings of revision 1 as revision 3", run.stdout)
        self.released()

    def test_a_picture_score_below_the_old_revision_puts_it_back(self):
        # A vision model on a smaller KV cache: text checks pass, pictures get worse.
        self.fake.engine.projector = True
        self.fake.write({"load": {"ctx_size": 16384, "parallel": 1}})
        self.fake.engine.misread = lambda cfg: cfg["cache_type_k"] == "q8_0"
        run = self.sb("set", TARGET, "cache_type_k=q8_0", "cache_type_v=q8_0", "--load", "--check",
                      "--runs", "12")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("picture baseline of revision 1", run.stdout)
        pictures = [r for r in self.fake.ledger if r["suite"] == "check-picture"]
        self.assertEqual([(r["settings_revision"], r["number_right"]) for r in pictures], [(1, 12), (2, 10)])
        self.assertEqual(pictures[-1]["stop_reason"], "below_baseline")
        texts = [r for r in self.fake.ledger if r["suite"] in ("check-long", "check-pool")]
        self.assertTrue(texts and all(r["passed"] == 1 for r in texts))
        self.assertEqual(self.fake.doc, {"load": {"ctx_size": 16384, "parallel": 1}})
        self.assertIn("picture failed (below_baseline)", self.fake.history[-1]["action"])

    def test_no_check_leaves_the_checks_for_the_next_load_which_reverts(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("set", TARGET, "engine_args.kv-unified=false", "--load", "--no-check")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("checks pending for revision 2", run.stdout)
        self.assertIn("warning: --no-check", run.stderr)
        self.assertIn("not checked", run.stderr)
        self.assertEqual(self.fake.engine.prompts, [])  # nothing was sent yet
        settings = self.sb("settings", TARGET)
        self.assertIn("checks pending for revision 2", settings.stdout)
        load = self.sb("load", TARGET, "--checks", "long")
        self.assertEqual(load.returncode, 1, load.stdout + load.stderr)
        self.assertIn("its checks run now", load.stdout)
        self.assertEqual(self.fake.doc["engine_args"], {"kv-unified": True})
        self.assertEqual(self.fake.revision, 3)
        # Checked now: the next load has nothing pending.
        again = self.sb("load", TARGET)
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertNotIn("checks run now", again.stdout)

    def test_a_revert_to_settings_that_passed_is_not_pending(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        self.check("--checks", "long")
        self.assertEqual(self.fake.ledger[-1]["passed"], 1)
        self.fake.write({"load": {"ctx_size": 30000, "parallel": 3}, "engine_args": {"kv-unified": True}})
        self.assertIn("checks pending for revision 2", self.sb("settings", TARGET).stdout)
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}},
                        action="revert:1")
        settings = self.sb("settings", TARGET)
        self.assertNotIn("checks pending", settings.stdout)

    def test_set_load_checks_a_guarded_change_by_default(self):
        # No --check: a memory or context change is checked at once all the same.
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}})
        run = self.sb("set", TARGET, "engine_args.kv-unified=true", "--load", "--checks", "long,pool")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("kept", run.stdout)
        puts = [b for m, p, b, h in self.fake.seen if m == "PUT"]
        self.assertEqual(puts[-1]["apply"], "now")
        self.assertTrue(self.fake.chats())
        self.released()

    def test_set_load_without_a_guarded_key_writes_for_the_next_load_unchecked(self):
        self.fake.write({"load": {"ctx_size": 16384}})
        run = self.sb("set", TARGET, "temp=0.5", "--load")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.fake.chats(), [])
        puts = [b for m, p, b, h in self.fake.seen if m == "PUT"]
        self.assertEqual(puts[-1]["apply"], "next_load")
        self.assertNotIn("checks pending", run.stdout)

    def test_a_guarded_change_for_the_next_load_needs_no_check(self):
        self.fake.write({"load": {"ctx_size": 16384}})
        run = self.sb("set", TARGET, "ctx_size=32768", "--load", "--apply", "next-load")
        self.assertEqual(run.returncode, 2)
        self.assertIn("--no-check", run.stderr)
        self.assertEqual(self.fake.revision, 1)

    def test_set_check_refuses_a_change_that_is_not_guarded(self):
        self.fake.write({"load": {"ctx_size": 16384}})
        run = self.sb("set", TARGET, "temp=0.5", "--load", "--check")
        self.assertEqual(run.returncode, 2)
        self.assertEqual(self.fake.revision, 1)


class ViewTests(Base):
    def test_settings_show_what_the_engine_runs_and_the_last_check_per_revision(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}})
        self.check("--checks", "long")
        run = self.sb("settings", TARGET)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("one request 8,192", run.stdout)
        self.assertIn("split over 3 slots", run.stdout)
        self.assertIn("checks, revision 1 (current): long FAIL (context_exceeded", run.stdout)

    def test_engines_and_doctor_name_a_split_context(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}})
        engines = self.sb("engines")
        self.assertIn("one request 8,192", engines.stdout)
        self.assertIn("set context split over slots", engines.stdout)
        doctor = self.sb("doctor", TARGET)
        self.assertIn("box/m: the set context 24,576 is split over 3 slots", doctor.stdout)
        self.assertIn("kv-unified=true", doctor.stdout)


def _lines(text):
    return [line for line in text.splitlines() if line.strip()]


class StopTests(Base):
    """Checks that stop after the write put the previous settings back and
    exit non-zero: an unchecked change never stays live."""

    def start(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        return json.loads(json.dumps(self.fake.doc))

    def assert_put_back(self, run, before, why):
        self.assertNotEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc, before, "the unchecked change stayed live")
        self.assertIn(why, self.fake.history[-1]["action"])
        self.assertIn("back to revision 1", self.fake.history[-1]["action"])
        self.released()

    def test_a_quiet_window_during_set_puts_the_old_settings_back(self):
        before = self.start()
        self.fake.quiet = "00:30-03:00"
        run = self.sb("set", TARGET, "ctx_size=98304", "--load", "--check", "--checks", "long,pool")
        self.assert_put_back(run, before, "quiet window 00:30-03:00 began")
        self.assertEqual(run.returncode, 1)
        self.assertIn("put back the settings of revision 1 as revision 3", run.stdout)

    def test_a_lost_lock_during_set_puts_the_old_settings_back(self):
        before = self.start()
        self.fake.lock_lost = True
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "long")
        self.assert_put_back(run, before, "the tuning lock is gone")

    def test_a_gateway_error_after_the_write_puts_the_old_settings_back(self):
        before = self.start()
        self.fake.load_fails = True
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "long")
        self.assert_put_back(run, before, "checks stopped: loading box/m")

    def test_ctrl_c_during_the_checks_puts_the_old_settings_back(self):
        before = self.start()
        sys.path.insert(0, str(CLI))
        import ai_gateway
        gw = ai_gateway.Gateway(self.fake.url, KEY)
        fails = (ai_gateway.Fail, ai_gateway.GatewayError)
        view = gw.get(f"/v1/models/{TARGET}/settings")["targets"][0]
        after = {"load": {"ctx_size": 30000, "parallel": 3}, "engine_args": {"kv-unified": True}}

        def write(extra):
            body = dict(after, **extra)
            return gw.call("PUT", f"/v1/models/{TARGET}/settings", body=body,
                           headers={"If-Match": str(view["revision"])})

        def interrupted(*_args, **_kwargs):
            raise KeyboardInterrupt

        args = ai_gateway.parser().parse_args(["set", TARGET, "ctx_size=30000", "--load", "--checks", "long"])
        original = ai_check.Checker.run_checks
        ai_check.Checker.run_checks = interrupted
        try:
            import io
            out, err = io.StringIO(), io.StringIO()
            code = ai_check.guard(gw, args, fails, view, write, after_doc=after, poll_secs=0.05,
                                  out=out, err=err)
        finally:
            ai_check.Checker.run_checks = original
        self.assertEqual(code, ai_check.INTERRUPTED)
        self.assertEqual(self.fake.doc, before)
        self.assertIn("checks stopped: interrupted", self.fake.history[-1]["action"])
        self.released()

    def test_nothing_is_written_when_the_quiet_window_begins_before_the_checks_can_end(self):
        self.start()
        self.fake.quiet_in = 600  # ten minutes
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "long,pool")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("the quiet window begins in 10 min", run.stderr)
        self.assertIn("nothing was written", run.stderr)
        self.assertEqual(self.fake.revision, 1)
        self.assertEqual(self.fake.chats(), [])
        self.released()

    def test_a_quiet_window_far_enough_away_lets_the_checks_run(self):
        self.start()
        self.fake.quiet_in = 3 * 3600
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "long")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc["load"]["ctx_size"], 30000)

    def pending_change(self):
        before = self.start()
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--no-check")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.fake.revision, 2)
        return before

    def test_a_quiet_window_during_pending_checks_puts_the_old_settings_back(self):
        before = self.pending_change()
        self.fake.quiet = "00:30-03:00"
        load = self.sb("load", TARGET, "--checks", "long")
        self.assertEqual(load.returncode, 1, load.stdout + load.stderr)
        self.assert_put_back(load, before, "quiet window 00:30-03:00 began")

    def test_pending_checks_that_cannot_start_put_the_old_settings_back(self):
        before = self.pending_change()
        self.fake.lock_refused = "quiet_window"
        load = self.sb("load", TARGET, "--checks", "long")
        self.assertEqual(load.returncode, 1, load.stdout + load.stderr)
        self.assertEqual(self.fake.doc, before)
        self.assertIn("checks stopped", self.fake.history[-1]["action"])

    def test_load_no_check_skips_the_pending_checks(self):
        self.pending_change()
        load = self.sb("load", TARGET, "--no-check")
        self.assertEqual(load.returncode, 0, load.stderr)
        self.assertEqual(self.fake.doc["load"]["ctx_size"], 30000)
        self.assertEqual(self.fake.chats(), [])


class MarginTests(Base):
    """The margin rule: the card memory a change leaves free, from the
    engine's own measurement, before any request."""

    def machines(self, **entry):
        path = self.root / "machines.json"
        path.write_text(json.dumps({"machines": [dict({"name": "gpu-box", "kind": "gpu"}, **entry)]}))
        self.env["SB_AI_MACHINES"] = str(path)

    def test_a_change_that_leaves_too_little_free_is_put_back_before_any_request(self):
        # 20 GB weights + 0.5 GB compute + 50 kB per token of KV on a 24 GB card.
        self.fake.engine.memory = {"weights": 20e9, "kv_per_token": 50_000, "compute": 0.5e9,
                                   "card": 24e9}
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        before = json.loads(json.dumps(self.fake.doc))
        run = self.sb("set", TARGET, "ctx_size=49152", "--load", "--checks", "long,pool")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc, before)
        self.assertEqual(self.fake.chats(), [])  # refused before any request
        self.assertIn("margin failed (margin_short)", self.fake.history[-1]["action"])
        self.assertIn("1.04 GB free, under the 1.50 GB", run.stdout)
        row = [r for r in self.fake.ledger if r["suite"] == "check-margin"][-1]
        self.assertEqual((row["passed"], row["stop_reason"], row["settings_revision"]), (0, "margin_short", 2))
        self.assertEqual((row["card_bytes"], row["free_bytes"]), (24_000_000_000, 1_042_400_000))
        self.released()

    def test_the_same_change_with_room_to_spare_passes(self):
        self.fake.engine.memory = {"weights": 18e9, "kv_per_token": 50_000, "compute": 0.5e9,
                                   "card": 24e9}
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("set", TARGET, "ctx_size=49152", "--load", "--checks", "long,pool")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("margin   PASS", run.stdout)
        self.assertEqual(self.fake.doc["load"]["ctx_size"], 49152)

    def test_the_card_size_and_its_other_users_come_from_the_machine_file(self):
        # The gateway does not pass the card's size on: the host's entry names it,
        # and the desktop and an image server hold 2 GB of it.
        self.fake.engine.memory = {"weights": 18e9, "kv_per_token": 50_000, "compute": 0.5e9}
        self.machines(vram_gb=24, others_gb=2, min_free_gb=1.5)
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("set", TARGET, "ctx_size=49152", "--load", "--checks", "long")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("vram_gb of host gpu-box in the machine file", run.stdout)
        self.assertIn("other users of the card 2.00 GB (others_gb of host gpu-box) = 1.04 GB free",
                      run.stdout)
        self.assertEqual(self.fake.chats(), [])
        # Flags win over the file: a smaller margin passes.
        run = self.sb("set", TARGET, "ctx_size=49152", "--load", "--checks", "long", "--min-free", "1")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)

    def test_other_loaded_models_count_against_the_card(self):
        self.fake.engine.memory = {"weights": 18e9, "kv_per_token": 50_000, "compute": 0.5e9,
                                   "card": 24e9}
        self.fake.others = [{"id": "embed", "status": "loaded", "args": [],
                             "memory": {"status": "measured", "effective": {"n_ctx": 2048,
                                                                           "bytes": 2_000_000_000}}},
                            {"id": "asleep", "status": "unloaded", "args": [],
                             "memory": {"status": "measured", "effective": {"n_ctx": 2048,
                                                                           "bytes": 9_000_000_000}}}]
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("check", TARGET, "--checks", "margin,long", "--json")
        report = json.loads(run.stdout)
        margin = self.by_check(report)["margin"]
        self.assertEqual(margin["margin"]["loaded_bytes"], 2_000_000_000)
        self.assertEqual(margin["passed"], True)  # 24 - 19.73 - 2 = 2.27 GB

    def test_without_a_measurement_the_margin_is_skipped_not_failed(self):
        self.fake.engine.memory = None
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run, report = self.check("--checks", "margin,long")
        self.assertEqual(run.returncode, 0, run.stderr)
        margin = self.by_check(report)["margin"]
        self.assertEqual((margin["passed"], margin["result"]), (None, "skipped"))
        self.assertFalse([r for r in self.fake.ledger if r["suite"] == "check-margin"])

    def test_a_larger_pool_whose_margin_cannot_be_judged_is_put_back_before_any_request(self):
        self.fake.engine.memory = None  # no measurement, and no fit ceiling to size the model
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        before = json.loads(json.dumps(self.fake.doc))
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "long,pool")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertEqual(self.fake.chats(), [])
        self.assertEqual(self.fake.doc, before)
        self.assertIn("margin could not judge", self.fake.history[-1]["action"])
        self.assertIn("sb ai fit REPO --store", run.stderr)
        self.released()

    def test_a_change_that_does_not_grow_the_pool_runs_without_a_margin(self):
        self.fake.engine.memory = None
        self.fake.write({"load": {"ctx_size": 30000, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("set", TARGET, "ctx_size=24576", "--load", "--checks", "long")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc["load"]["ctx_size"], 24576)


class GatewayMachineTests(Base):
    """The machine facts live in the gateway: a client with no machine file
    gets the same margin verdict, and a local file still wins."""

    ENTRY = {"name": "gpu-box", "kind": "gpu", "vram_gb": 24, "others_gb": 2, "min_free_gb": 1.5}

    def margin(self, *extra):
        self.fake.engine.memory = {"weights": 18e9, "kv_per_token": 50_000, "compute": 0.5e9}
        self.fake.write({"load": {"ctx_size": 49152, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("check", TARGET, "--checks", "margin", "--json", *extra)
        self.assertIn(run.returncode, (0, 1), run.stderr)
        return self.by_check(json.loads(run.stdout))["margin"]

    def test_without_a_machine_file_the_gateway_facts_give_the_same_verdict(self):
        path = self.root / "machines.json"
        path.write_text(json.dumps({"machines": [self.ENTRY]}))
        self.env["SB_AI_MACHINES"] = str(path)
        local = self.margin()
        del self.env["SB_AI_MACHINES"]
        none = self.margin()
        self.assertIsNone(none["passed"])  # nothing knows the card: skipped
        self.fake.machines = [dict(self.ENTRY, source="gateway")]
        served = self.margin()
        self.assertEqual(local["passed"], False)
        self.assertEqual((served["passed"], served["margin"]["free_bytes"]),
                         (local["passed"], local["margin"]["free_bytes"]))
        self.assertIn("in the gateway's config", served["margin"]["card_from"])
        self.assertIn("in the gateway's config", served["margin"]["min_free_from"])

    def test_a_local_machine_file_overrides_the_gateway(self):
        self.fake.machines = [dict(self.ENTRY, vram_gb=48)]
        served = self.margin()
        self.assertEqual(served["passed"], True)
        path = self.root / "machines.json"
        path.write_text(json.dumps({"machines": [self.ENTRY]}))
        self.env["SB_AI_MACHINES"] = str(path)
        local = self.margin()
        self.assertEqual(local["passed"], False)
        self.assertIn("in the machine file", local["margin"]["card_from"])

    def test_a_unified_memory_host_is_judged_on_its_usable_memory(self):
        self.fake.machines = [{"name": "gpu-box", "kind": "unified", "usable_gb": 40,
                               "others_gb": 12, "min_free_gb": 6}]
        margin = self.margin()
        self.assertEqual(margin["passed"], True, margin)
        self.assertIn("usable_gb (unified memory)", margin["margin"]["card_from"])


class StoreLockTests(Base):
    """A store lock on a load key: refused at set time with its reason,
    before any hold or write; lock and unlock are writes of their own."""

    def locked_doc(self):
        self.fake.write({"load": {"ctx_size": 32768}, "locked": ["load.ctx_size"],
                         "lock_reasons": {"load.ctx_size": "night job uses this"}})

    def test_a_locked_load_key_is_refused_before_anything_is_held_or_written(self):
        self.locked_doc()
        revision = self.fake.revision
        run = self.sb("set", TARGET, "ctx_size=65536", "--load")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("is locked (night job uses this)", run.stderr)
        self.assertIn(f"sb ai unlock {TARGET} ctx_size", run.stderr)
        self.assertEqual(self.fake.revision, revision)
        self.assertEqual(self.fake.holds, {})
        self.assertEqual(self.fake.locks, {})
        self.assertEqual(self.fake.chats(), [])
        # unset of the key changes it too.
        run = self.sb("unset", TARGET, "ctx_size", "--load")
        self.assertEqual(run.returncode, 1)
        self.assertIn("is locked", run.stderr)
        self.assertEqual(self.fake.revision, revision)
        # Another load key is not locked.
        run = self.sb("set", TARGET, "batch_size=1024", "--load", "--no-check")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.revision, revision + 1)

    def test_lock_and_unlock_are_writes_of_their_own(self):
        self.fake.write({"load": {"ctx_size": 32768}})
        run = self.sb("lock", TARGET, "ctx_size", "--reason", "night job uses this")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc["locked"], ["load.ctx_size"])
        self.assertEqual(self.fake.doc["lock_reasons"], {"load.ctx_size": "night job uses this"})
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 32768})
        run = self.sb("lock", TARGET, "engine_args.model")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc["locked"], ["load.ctx_size", "engine_args.model"])
        run = self.sb("unlock", TARGET, "ctx_size")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc["locked"], ["engine_args.model"])
        self.assertNotIn("lock_reasons", self.fake.doc)
        self.assertEqual(self.fake.doc["load"], {"ctx_size": 32768})
        run = self.sb("unlock", TARGET, "ctx_size")
        self.assertEqual(run.returncode, 2)
        self.assertIn("not locked", run.stderr)
        for bad in (["lock", TARGET, "nonsense_key"], ["lock", TARGET, "engine_args.a b"],
                    ["lock", TARGET, "ctx_size", "--reason", ""]):
            self.assertEqual(self.sb(*bad).returncode, 2, bad)

    def test_the_gateway_refusal_is_shown_as_it_says(self):
        # A lock the client does not see (another spelling of the argument)
        # is still refused by the gateway, with its own words.
        self.fake.write({"load": {"ctx_size": 32768}, "locked": ["load.ctx_size"]})
        self.fake.doc["locked"] = []  # the client reads no lock ...
        view = self.fake.view

        def locked_view():
            answer = view()
            answer["document"] = dict(answer["document"], locked=[])
            return answer
        self.fake.view = locked_view
        self.fake.doc["locked"] = ["load.ctx_size"]  # ... the gateway has one
        run = self.sb("set", TARGET, "ctx_size=65536", "--load", "--no-check")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("is locked (gateway)", run.stderr)


class FitMarginTests(Base):
    """Without an engine measurement, the margin sizes the model with `sb ai
    fit`: the running file's weights, the pool's KV cache from the file
    header, and the host's compute buffers."""

    def setUp(self):
        super().setUp()
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        import sb_ai_fit as fixtures
        listing = fixtures.MOE_LISTING
        headers = {(listing["id"], f["rfilename"]): fixtures.moe_header()
                   for f in listing["siblings"] if f["rfilename"].endswith(".gguf")}
        self.index = fixtures.Index({listing["id"]: listing}, {}, headers)
        self.addCleanup(self.index.close)
        self.env["SB_AI_FIT_HF_BASE"] = self.index.url
        # The card as measured on a 24 GB box: 28 GB here, others 2.1 GB, the
        # runtime's own buffers 0.85 GB, and 0.5 GB that must stay free.
        path = self.root / "machines.json"
        path.write_text(json.dumps({"machines": [{"name": "gpu-box", "kind": "gpu", "vram_gb": 28,
                                                  "others_gb": 2.1, "compute_gb": 0.85,
                                                  "min_free_gb": 0.5}]}))
        self.env["SB_AI_MACHINES"] = str(path)
        engine = self.fake.engine
        engine.memory = None
        engine.no_file = True
        engine.model_path = "/models/moe-35b-Q4_K_M.gguf"  # the file that runs
        # The stored fit ceiling names the repository (its quant is not the one that runs).
        self.ceiling = {"bytes_per_token": 1, "source": "sb ai fit test/moe-35b Q8_0 at 1008 GB/s"}

    def test_a_pool_that_leaves_room_passes_and_one_that_does_not_is_put_back(self):
        self.fake.write({"load": {"ctx_size": 32768, "parallel": 3}, "engine_args": {"kv-unified": True},
                         "ceiling": self.ceiling})
        run = self.sb("set", TARGET, "ctx_size=98304", "--load", "--checks", "long")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("fit test/moe-35b Q4_K_M: weights 21.71 GB + KV 2.21 GB at 98,304 f16/f16 "
                      "+ compute 0.85 GB", run.stdout)
        self.assertIn("= 1.12 GB free", run.stdout)
        kept = json.loads(json.dumps(self.fake.doc))
        sent = len(self.fake.chats())
        run = self.sb("set", TARGET, "ctx_size=131072", "--load", "--checks", "long,pool")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("KV 2.95 GB at 131,072", run.stdout)
        self.assertIn("0.38 GB free, under the 0.50 GB", run.stdout)
        self.assertEqual(len(self.fake.chats()), sent)  # no request at 131,072
        self.assertEqual(self.fake.doc, kept)
        self.assertIn("margin failed (margin_short)", self.fake.history[-1]["action"])
        # The model index was read once per pool size.
        listings = [r for r in self.index.seen if r["path"].startswith("/api/models/")]
        self.assertEqual(len(listings), 2)
        self.released()

    def test_a_smaller_kv_cache_type_is_sized_by_its_bytes(self):
        self.fake.write({"load": {"ctx_size": 131072, "parallel": 3}, "engine_args": {"kv-unified": True},
                         "ceiling": self.ceiling}, )
        run = self.sb("set", TARGET, "cache_type_k=q8_0", "cache_type_v=q8_0", "--load", "--checks", "long")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("at 131,072 q8_0/q8_0", run.stdout)

    def test_another_loaded_model_without_a_measurement_leaves_the_margin_unjudged(self):
        self.fake.others = [{"id": "embed", "status": "loaded", "args": [], "memory": None}]
        self.fake.write({"load": {"ctx_size": 32768, "parallel": 3}, "engine_args": {"kv-unified": True},
                         "ceiling": self.ceiling})
        before = json.loads(json.dumps(self.fake.doc))
        run = self.sb("set", TARGET, "ctx_size=65536", "--load", "--checks", "long")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("embed is loaded on the same engine with no memory measurement", run.stdout)
        self.assertEqual(self.fake.doc, before)


class RepeatTests(Base):
    """A change that grows the shared pool needs every pool repeat to pass,
    each after the first on a fresh process."""

    def crash_on_the_third_process(self):
        # An out-of-memory crash that comes only on some fresh processes: the
        # third process after the write dies on its first long request.
        engine = self.fake.engine
        start = {}

        def rule(cfg, tokens):
            start.setdefault("port", engine.port)
            return engine.port == start["port"] + 2 and tokens > 5000

        engine.crash_rule = rule

    def test_one_pool_pass_misses_an_intermittent_crash(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        self.crash_on_the_third_process()
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "pool", "--pool-repeats", "1")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertEqual(self.fake.doc["load"]["ctx_size"], 30000)

    def test_three_pool_passes_catch_it_and_put_the_old_settings_back(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        before = json.loads(json.dumps(self.fake.doc))
        self.crash_on_the_third_process()
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "pool")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("ctx_size 24,576 -> 30,000", run.stdout)
        self.assertIn("pool 3/3 failed", self.fake.history[-1]["action"])
        self.assertEqual(self.fake.doc, before)
        pools = [r for r in self.fake.ledger if r["suite"] == "check-pool"]
        self.assertEqual([(r["repeat"], r["passed"]) for r in pools], [(1, 1), (2, 1), (3, 0)])
        self.released()

    def test_a_change_that_shrinks_the_pool_runs_the_pool_check_once(self):
        self.fake.write({"load": {"ctx_size": 30000, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("set", TARGET, "ctx_size=24576", "--load", "--checks", "pool")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        pools = [r for r in self.fake.ledger if r["suite"] == "check-pool"]
        self.assertEqual(len(pools), 1)
        self.assertNotIn("repeat", pools[0])

    def test_a_pool_repeat_that_cannot_judge_does_not_approve_a_larger_pool(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        before = json.loads(json.dumps(self.fake.doc))
        self.fake.engine.capped = True
        run = self.sb("set", TARGET, "ctx_size=30000", "--load", "--checks", "pool")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("pool passed 0 of 3", self.fake.history[-1]["action"])
        self.assertEqual(self.fake.doc, before)


class GrowthTests(unittest.TestCase):
    def test_what_grows_the_shared_pool(self):
        base = {"load": {"ctx_size": 98304, "parallel": 3}, "engine_args": {"kv-unified": True}}
        grow = ai_check.pool_growth
        self.assertEqual(grow(base, {"load": {"ctx_size": 131072, "parallel": 3},
                                     "engine_args": {"kv-unified": True}}), ["ctx_size 98,304 -> 131,072"])
        self.assertEqual(grow(base, {"load": {"ctx_size": 65536, "parallel": 3},
                                     "engine_args": {"kv-unified": True}}), [])
        self.assertEqual(grow({"load": {"ctx_size": 98304, "parallel": 3}}, base), ["kv-unified on"])
        self.assertEqual(grow(base, {"load": {"ctx_size": 98304, "parallel": 4},
                                     "engine_args": {"kv-unified": True}}), ["parallel 3 -> 4"])
        smaller = {"load": dict(base["load"], cache_type_k="q8_0"), "engine_args": {"kv-unified": True}}
        self.assertEqual(grow(base, smaller), ["cache_type_k f16 -> q8_0"])
        self.assertEqual(grow(smaller, base), [])
        # Unset: the engine's own context, which may be far larger.
        self.assertEqual(grow(base, {"load": {"parallel": 3}, "engine_args": {"kv-unified": True}}),
                         ["ctx_size 98,304 -> the engine's own"])
        # What the engine runs now fills in what the document leaves out.
        truth = {"ctx_size": 98304, "slots": 3, "kv_unified": True}
        self.assertEqual(grow({}, {"load": {"ctx_size": 65536}}, truth), [])
        self.assertEqual(grow({}, {"load": {"ctx_size": 131072}}, truth), ["ctx_size 98,304 -> 131,072"])

    def test_the_margin_reads_the_engines_measurement(self):
        memory = {"status": "measured",
                  "min": {"n_ctx": 4096, "bytes": 20_000_000_000, "context_bytes": 200_000_000,
                          "total_bytes": 24_000_000_000}}
        got = ai_check.measured(memory, 8192)
        self.assertEqual((got["bytes"], got["card"]), (20_200_000_000, 24_000_000_000))
        self.assertIsNone(ai_check.measured({"status": "unmeasured"}, 8192))
        self.assertIsNone(ai_check.measured(None))
        # An older gateway: no KV part to scale, so only a measurement at the
        # running context counts.
        old = {"min": {"n_ctx": 4096, "bytes": 20_000_000_000}}
        self.assertIsNone(ai_check.measured(old, 8192))
        self.assertEqual(ai_check.measured(old, 4096)["bytes"], 20_000_000_000)


if __name__ == "__main__":
    unittest.main()
