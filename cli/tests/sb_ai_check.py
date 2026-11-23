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

    def row(self):
        return {"id": "m", "target": TARGET, "file": "file-a", "status": self.status,
                "failed": self.failed, "args": self.args()}


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
            with self.engine.lock:
                if self.engine.status not in ("loaded", "sleeping"):
                    self.engine.restart()
            return 200, {"target": TARGET, "model": "m", "lane": "box", "status": "loaded"}
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
            return 200, {"object": "list", "data": [{"lane": "box", "build": "b100", "models": [row]}]}
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
            lock_id = f"lock-{len(self.locks) + 1}"
            self.locks[lock_id] = body
            return 201, {"id": lock_id}
        if method == "PATCH" and path.startswith("/v1/optimize/locks/"):
            return 200, {"id": path.rsplit("/", 1)[1], "quiet_window": None}
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
        # Like revision 54: 98,304 over 3 slots, kv-unified off.
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
        # Like revision 57: loads, then the first long request runs out of
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
        # Like hauhau35 revision 4: text checks pass, pictures get worse.
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

    def test_without_check_the_checks_wait_for_the_next_load_and_revert_there(self):
        self.fake.write({"load": {"ctx_size": 24576, "parallel": 3}, "engine_args": {"kv-unified": True}})
        run = self.sb("set", TARGET, "engine_args.kv-unified=false", "--load")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("checks pending for revision 2", run.stdout)
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


if __name__ == "__main__":
    unittest.main()
