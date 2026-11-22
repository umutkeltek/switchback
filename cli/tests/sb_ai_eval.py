#!/usr/bin/env python3
"""sb ai eval against a fake gateway on a local port.

The fake records every request (body and headers) and answers each case from
a table, so the quality of each model is known before the run: a case's
answer names k of its ten codes and scores k/10 under the built-in checks.
Every run goes through the real `sb` dispatcher in a child process."""

import json
import os
import random
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
KEY = "sk-test-5e1f00aa"
sys.path.insert(0, str(CLI))
import ai_eval  # noqa: E402

CASE_RE = re.compile(r"CASE ([A-Za-z0-9_.:-]+):")


class Fake:
    def __init__(self):
        self.lock = threading.Lock()
        self.seen = []
        self.targets = {
            "box/m": {"lane": "box", "file": "file-m", "hash": "hash-m-0001", "revision": 4},
            "lane/a": {"lane": "lane", "file": "file-a", "hash": "hash-a-0001", "revision": 2},
            "lane/b": {"lane": "lane", "file": "file-b", "hash": "hash-b-0001", "revision": 7},
        }
        self.routes = {"both": ["lane/a", "lane/b"]}
        self.lanes = {"box": {"host": "gpu-box", "state": "healthy", "experiments": True},
                      "lane": {"host": "lane-host", "state": "healthy", "experiments": True}}
        self.builds = {"box": "b100", "lane": "b7"}
        self.answers = {}            # (target, case id) -> (text, finish)
        self.default = ("", "stop")
        self.quality = []
        self.locks = {}
        self.holds = {}
        self.lock_refusal = None
        self.quiet_after_beats = None
        self.beats = 0
        self.chat_delay = 0.0
        self.chat_started = threading.Event()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def target_view(self, target):
        t = self.targets[target]
        return {"target": target, "revision": t["revision"], "settings_hash": t["hash"], "document": {},
                "load": {"engine": {"lane": t["lane"], "kind": "llama_cpp_router"}}}

    def chat(self, body):
        self.chat_started.set()
        if self.chat_delay:
            time.sleep(self.chat_delay)
        user = body["messages"][-1]["content"]
        match = CASE_RE.search(user)
        case_id = match.group(1) if match else "?"
        text, finish = self.answers.get((body["model"], case_id), self.default)
        if callable(text):
            text = text()
        return 200, {"id": "x", "object": "chat.completion", "model": body["model"],
                     "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                                  "finish_reason": finish}],
                     "usage": {"prompt_tokens": len(user) // 4, "completion_tokens": len(text) // 4}}

    def route(self, method, path, body):
        parsed = urllib.parse.urlsplit(path)
        query = dict(urllib.parse.parse_qsl(parsed.query))
        path = parsed.path
        m = re.match(r"^/v1/models/(.+)/settings$", path)
        if method == "GET" and m:
            name = m.group(1)
            if name in self.routes:
                return 200, {"model": name, "targets": [self.target_view(t) for t in self.routes[name]]}
            if name not in self.targets:
                return 404, {"error": {"message": f"unknown model {name}", "type": "not_found"}}
            return 200, {"model": name, "targets": [self.target_view(name)],
                         "revision": self.targets[name]["revision"]}
        m = re.match(r"^/v1/models/(.+)/load$", path)
        if method == "POST" and m:
            return 200, {"target": m.group(1), "status": "loaded"}
        if method == "GET" and path == "/v1/workloads/capacity":
            return 200, {"object": "list", "data": [
                {"name": name, "host": lane["host"], "state": lane["state"], "level": "ok",
                 "queue_depth": 0, "in_flight": 0, "experiments": lane["experiments"]}
                for name, lane in self.lanes.items()]}
        if method == "GET" and path == "/v1/engines":
            return 200, {"object": "list", "data": [
                {"lane": lane, "build": self.builds[lane],
                 "models": [{"id": t.split("/", 1)[1], "file": v["file"]}
                            for t, v in self.targets.items() if v["lane"] == lane]}
                for lane in self.lanes]}
        m = re.match(r"^/v1/hosts/([^/]+)/holds$", path)
        if method == "POST" and m:
            hold_id = f"hold-{len(self.holds) + 1}-{m.group(1)}"
            self.holds[hold_id] = body
            if body.get("wake"):
                for lane in self.lanes.values():
                    if lane["host"] == m.group(1):
                        lane["state"] = "idle_countdown"
            return 201, {"id": hold_id, "host": m.group(1), "state": "active"}
        if method == "DELETE" and re.match(r"^/v1/hosts/[^/]+/holds/", path):
            self.holds.pop(path.rsplit("/", 1)[1], None)
            return 200, {"deleted": True}
        if method == "POST" and path == "/v1/optimize/locks":
            if self.lock_refusal:
                reason, message = self.lock_refusal
                return 409, {"error": {"message": message, "type": reason, "reason": reason}}
            lock_id = f"tune-lock-{len(self.locks) + 1}"
            self.locks[lock_id] = body
            return 201, {"id": lock_id, "target": body["target"]}
        if method == "PATCH" and path.startswith("/v1/optimize/locks/"):
            lock_id = path.rsplit("/", 1)[1]
            self.beats += 1
            if lock_id not in self.locks:
                return 404, {"error": {"message": "no lock", "type": "not_found", "reason": "not_found"}}
            window = None
            if self.quiet_after_beats is not None and self.beats >= self.quiet_after_beats:
                window = "00:30-03:00"
            return 200, {"id": lock_id, "quiet_window": window}
        if method == "DELETE" and path.startswith("/v1/optimize/locks/"):
            self.locks.pop(path.rsplit("/", 1)[1], None)
            return 200, {"deleted": True}
        if method == "POST" and path == "/v1/optimize/quality":
            row = dict(body, id=len(self.quality) + 1, at_ms=int(time.time() * 1000) + len(self.quality))
            self.quality.append(row)
            return 201, {"id": row["id"], "durable": True}
        if method == "GET" and path == "/v1/optimize/quality":
            rows = [r for r in self.quality
                    if r.get("target") == query.get("model", r.get("target"))
                    and r.get("suite") == query.get("suite", r.get("suite"))]
            return 200, {"object": "list", "durable": True,
                         "data": sorted(rows, key=lambda r: r["at_ms"], reverse=True)}
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
                    status, payload = fake.chat(body)
                else:
                    status, payload = fake.route(self.command, self.path, body)
                data = json.dumps(payload).encode() if payload is not None else b""
                self.send_response(status)
                if payload is not None:
                    self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = serve

        return Handler

    def calls(self, method=None, prefix=""):
        with self.lock:
            return [(m, p, b, h) for m, p, b, h in self.seen
                    if (method is None or m == method) and p.startswith(prefix)]

    def chats(self):
        return self.calls("POST", "/v1/chat/completions")


def codes(k):
    """An answer that names k of a case's ten codes (scores k/10)."""
    return " ".join(f"K{j}" for j in range(k)) or "none"


def case(case_id, **request):
    body = {"messages": [{"role": "system", "content": "You check things.\nKeep it short."},
                         {"role": "user", "content": f"CASE {case_id}: name the codes  éè \"quoted\"\n"}],
            "max_tokens": 300, "temperature": 0, "stream": False,
            "response_format": {"type": "json_object"}, "reasoning_effort": "none"}
    body.update(request)
    return {"id": case_id, "request": body,
            "expect": {"checks": [{"contains_all": [f"K{j}"]} for j in range(10)]}, "tags": []}


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-ai-eval-")
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name).resolve()
        (root / "home").mkdir()
        (root / "runtime/config").mkdir(parents=True)
        (root / "runtime/manifest.json").write_text("{}")
        (root / "xdg/switchback").mkdir(parents=True)
        (root / "suites").mkdir(mode=0o755)
        (root / "runs").mkdir(mode=0o700)
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
            "SB_AI_SUITES": str(root / "suites"),
            "SB_AI_EVAL_RUNS": str(root / "runs"),
            "NO_PROXY": "*", "no_proxy": "*",
        }

    def suite(self, name, cases, **meta):
        path = self.root / "suites" / name
        path.mkdir(mode=0o755)
        os.chmod(path, 0o755)
        settings = {"name": name, "kind": "chat", "scorer": "checks", "concurrency": 1, "repeats": 2,
                    "min_effect": 0.02, "roles": []}
        settings.update(meta)
        (path / "suite.json").write_text(json.dumps(settings, indent=2))
        with open(path / "cases.jsonl", "w", encoding="utf-8") as handle:
            for c in cases:
                handle.write(json.dumps(c) + "\n")
        return path

    def sb(self, *args, timeout=240):
        run = subprocess.run(["zsh", str(SB), "ai", *args], env=self.env, cwd=self.root,
                             capture_output=True, text=True, timeout=timeout)
        self.assertNotIn(KEY, run.stdout + run.stderr)
        return run

    def eval_json(self, *args):
        run = self.sb("eval", *args, "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        return json.loads(run.stdout)

    def assert_released(self):
        self.assertEqual(self.fake.locks, {}, "the tuning lock was not released")
        self.assertEqual(self.fake.holds, {}, "the hold was not released")

    def store_row(self, target, suite_path, per_case, **extra):
        t = self.fake.targets[target]
        row = {"run_id": f"eval_{len(self.fake.quality) + 1:04d}", "target": target,
               "suite": suite_path.name, "suite_version": ai_eval.suite_version(suite_path),
               "model_file": t["file"], "engine_build": self.fake.builds[t["lane"]],
               "settings_hash": t["hash"], "settings_revision": t["revision"], "status": "complete",
               "score": 0.5, "se": 0.01, "per_case": per_case}
        row.update(extra)
        self.fake.route("POST", "/v1/optimize/quality", row)
        return row


class RunTests(Base):
    def test_case_request_is_sent_byte_for_byte(self):
        cases = [case(f"c{i:02d}") for i in range(10)]
        path = self.suite("s", cases)
        for c in cases:
            self.fake.answers[("box/m", c["id"])] = (codes(10), "stop")
        report = self.eval_json("box/m", "--suite", "s")
        self.assertEqual(report["runs"][0]["status"], "complete")
        chats = self.fake.chats()
        self.assertEqual(len(chats), 20)
        lock_id = next(iter(self.fake.calls("POST", "/v1/optimize/locks")))
        self.assertTrue(lock_id)
        by_id = {c["id"]: c for c in cases}
        for _, _, body, headers in chats:
            case_id = CASE_RE.search(body["messages"][-1]["content"]).group(1)
            sent = dict(body)
            self.assertEqual(sent.pop("model"), "box/m")
            self.assertEqual(sent.pop("switchback"), {"settings": {"seed": 1}})
            # Everything else is the case's request, byte for byte.
            self.assertEqual(sent, by_id[case_id]["request"])
            self.assertEqual(json.dumps(sent["messages"]), json.dumps(by_id[case_id]["request"]["messages"]))
            self.assertTrue(headers.get("x-switchback-tune-lock", "").startswith("tune-lock-"))
        self.assertEqual(report["runs"][0]["suite_version"], ai_eval.suite_version(path))
        self.assert_released()

    def test_every_repeat_reaches_the_engine(self):
        cases = [case(f"c{i:02d}") for i in range(10)]
        self.suite("s", cases)
        for c in cases:
            self.fake.answers[("box/m", c["id"])] = (codes(7), "stop")
        report = self.eval_json("box/m", "--suite", "s")
        chats = self.fake.chats()
        self.assertEqual(len(chats), 20)
        for _, _, body, _ in chats:
            self.assertEqual(body["switchback"]["settings"], {"seed": 1})
        row = report["runs"][0]
        self.assertAlmostEqual(row["score"], 0.7)
        self.assertEqual(len(row["per_case"]), 10)
        self.assertTrue(all(len(v) == 2 for v in row["per_case"].values()))
        self.assertEqual(row["nondeterministic_share"], 0.0)
        # The stored row carries scores and ids, no text.
        stored = self.fake.quality[-1]
        self.assertIsNone(ai_eval.text_problem(stored))
        self.assertNotIn("K6", json.dumps(stored))
        self.assertNotIn("CASE", json.dumps(stored))
        # The answers stay in the private runs folder.
        answers = (self.root / "runs" / row["run_id"] / "answers.jsonl").read_text()
        self.assertIn("K6", answers)
        self.assertEqual(len(answers.splitlines()), 20)

    def test_cut_answer_is_a_hard_fail(self):
        cases = [case("c00"), case("c01")]
        self.suite("s", cases)
        # Valid JSON, cut at the token limit: still a hard fail.
        self.fake.answers[("box/m", "c00")] = ('{"codes": "' + codes(10) + '"}', "length")
        self.fake.answers[("box/m", "c01")] = (codes(10), "stop")
        row = self.eval_json("box/m", "--suite", "s")["runs"][0]
        self.assertEqual(row["per_case"]["c00"], [0.0, 0.0])
        self.assertEqual(row["per_case"]["c01"], [1.0, 1.0])
        self.assertEqual(row["hard_fails"], {"cut": 2})
        self.assertEqual(row["case_fail"], {"c00": [{"cut": 1}, {"cut": 1}]})
        self.assertIsNone(ai_eval.text_problem(self.fake.quality[-1]))
        self.assertAlmostEqual(row["score"], 0.5)

    def test_loop_and_empty_are_hard_fails(self):
        cases = [case("c00"), case("c01"), case("c02")]
        self.suite("s", cases)
        loop = (codes(10) + " ") + " ".join(["the same six words come again"] * 8)
        self.fake.answers[("box/m", "c00")] = (loop, "stop")
        self.fake.answers[("box/m", "c01")] = ("   ", "stop")
        self.fake.answers[("box/m", "c02")] = (codes(10), "stop")
        row = self.eval_json("box/m", "--suite", "s")["runs"][0]
        self.assertEqual(row["hard_fails"], {"loop": 2, "empty": 2})
        self.assertEqual(row["per_case"]["c00"], [0.0, 0.0])
        self.assertEqual(row["per_case"]["c01"], [0.0, 0.0])

    def test_nondeterministic_answers_add_a_repeat(self):
        cases = [case(f"c{i:02d}") for i in range(6)]
        self.suite("s", cases)
        counter = iter(range(10_000))
        self.fake.answers[("box/m", "c00")] = (lambda: codes(5) + f" v{next(counter)}", "stop")
        for c in cases[1:]:
            self.fake.answers[("box/m", c["id"])] = (codes(5), "stop")
        run = self.sb("eval", "box/m", "--suite", "s", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        row = json.loads(run.stdout)["runs"][0]
        self.assertTrue(row["extra_repeat"])
        self.assertIn("one more repeat", run.stderr)
        self.assertEqual(len(row["per_case"]["c00"]), 2)
        self.assertEqual(len(row["per_case"]["c05"]), 3)
        self.assertEqual(len(self.fake.chats()), 2 + 5 * 3)

    def test_a_route_with_several_targets_is_refused(self):
        self.suite("s", [case("c00")])
        run = self.sb("eval", "both", "--suite", "s")
        self.assertEqual(run.returncode, 2, run.stderr)
        self.assertIn("eval measures targets, not roles", run.stderr)
        self.assertEqual(self.fake.calls("POST", "/v1/hosts"), [])


class SafetyTests(Base):
    def test_sleeping_machine_without_wake_sends_nothing(self):
        self.suite("s", [case("c00")])
        self.fake.lanes["box"]["state"] = "offline"
        run = self.sb("eval", "box/m", "--suite", "s")
        self.assertEqual(run.returncode, 3, run.stderr)
        self.assertIn("--wake", run.stderr)
        self.assertEqual(self.fake.chats(), [])
        self.assertEqual([c for c in self.fake.calls("POST") if c[1].endswith("/load")], [])
        self.assertEqual(self.fake.calls("POST", "/v1/hosts"), [])
        self.assertEqual(self.fake.calls("POST", "/v1/optimize/locks"), [])

    def test_wake_holds_and_waits_for_the_machine(self):
        self.suite("s", [case("c00")])
        self.fake.answers[("box/m", "c00")] = (codes(10), "stop")
        self.fake.lanes["box"]["state"] = "offline"
        report = self.eval_json("box/m", "--suite", "s", "--wake")
        self.assertEqual(report["runs"][0]["status"], "complete")
        hold = self.fake.calls("POST", "/v1/hosts/gpu-box/holds")[0][2]
        self.assertTrue(hold["wake"])
        self.assertTrue(hold["until_quiet_window"])
        self.assert_released()

    def test_quiet_window_refusal_sends_nothing(self):
        self.suite("s", [case("c00")])
        self.fake.lock_refusal = ("quiet_window", "inside the quiet window 00:30-03:00")
        run = self.sb("eval", "box/m", "--suite", "s")
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("quiet window", run.stderr)
        self.assertEqual(self.fake.chats(), [])
        self.assertEqual(len(self.fake.calls("POST", "/v1/hosts/gpu-box/holds")), 1)
        self.assertEqual(len(self.fake.calls("DELETE", "/v1/hosts/gpu-box/holds/")), 1)
        self.assert_released()
        self.assertEqual(self.fake.quality, [])

    def test_experiments_off_lane_is_refused_before_any_hold(self):
        self.suite("s", [case("c00")])
        self.fake.lanes["box"]["experiments"] = False
        self.fake.lanes["box"]["state"] = "offline"
        run = self.sb("eval", "box/m", "--suite", "s", "--wake")
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("experiments: false", run.stderr)
        self.assertEqual(self.fake.calls("POST", "/v1/hosts"), [])
        self.assertEqual(self.fake.calls("POST", "/v1/optimize/locks"), [])
        self.assertEqual(self.fake.chats(), [])
        self.assertEqual(self.fake.lanes["box"]["state"], "offline")

    def test_quiet_window_mid_run_stores_partial(self):
        cases = [case(f"c{i:02d}") for i in range(10)]
        path = self.suite("s", cases)
        for c in cases:
            self.fake.answers[("lane/a", c["id"])] = (codes(9), "stop")
        self.fake.chat_delay = 0.35
        self.fake.quiet_after_beats = 1
        run = self.sb("eval", "lane/a", "--suite", "s", "--repeats", "1", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("quiet window", run.stderr)
        row = json.loads(run.stdout)["runs"][0]
        self.assertEqual(row["status"], "partial")
        self.assertIsNone(row["score"])
        sent = len(self.fake.chats())
        self.assertGreater(sent, 0)
        self.assertLess(sent, 10)
        self.assertEqual(self.fake.quality[-1]["status"], "partial")
        self.assert_released()
        # --compare never uses a partial row.
        self.store_row("lane/b", path, {f"c{i:02d}": [0.5] for i in range(10)})
        report = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s", "--any")
        self.assertEqual(report["verdict"]["verdict"], "no_verdict")
        self.assertIn("lane/a", report["verdict"]["text"])

    def test_sigterm_releases_lock_and_hold(self):
        cases = [case(f"c{i:02d}") for i in range(10)]
        self.suite("s", cases)
        for c in cases:
            self.fake.answers[("box/m", c["id"])] = (codes(9), "stop")
        self.fake.chat_delay = 0.5
        proc = subprocess.Popen(["zsh", str(SB), "ai", "eval", "box/m", "--suite", "s"],
                                env=self.env, cwd=self.root, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        self.assertTrue(self.fake.chat_started.wait(60))
        time.sleep(0.1)
        proc.send_signal(signal.SIGTERM)
        out, err = proc.communicate(timeout=60)
        self.assertEqual(proc.returncode, 130, err)
        self.assertIn("SIGTERM", err)
        self.assertEqual(len(self.fake.calls("DELETE", "/v1/optimize/locks/")), 1)
        self.assertEqual(len(self.fake.calls("DELETE", "/v1/hosts/gpu-box/holds/")), 1)
        self.assert_released()
        self.assertLess(len(self.fake.chats()), 20)

    def test_runs_folder_inside_a_repo_is_refused(self):
        self.suite("s", [case("c00")])
        repo = self.root / "work" / "repo"
        (repo / ".git").mkdir(parents=True)
        self.env["SB_AI_EVAL_RUNS"] = str(repo / "nested" / "runs")
        run = self.sb("eval", "box/m", "--suite", "s")
        self.assertEqual(run.returncode, 2, run.stderr)
        self.assertIn("git work tree", run.stderr)
        self.assertEqual(self.fake.calls("POST"), [])
        self.assertFalse((repo / "nested").exists())

    def test_world_writable_plugin_is_refused(self):
        path = self.suite("p", [case("c00")], scorer="plugin")
        plugin = path / "score.py"
        plugin.write_text("def score(case, answer):\n    return {'score': 1.0, 'hard_fail': None, 'detail': {}}\n")
        os.chmod(plugin, 0o666)
        run = self.sb("eval", "box/m", "--suite", "p")
        self.assertEqual(run.returncode, 2, run.stderr)
        self.assertIn("writable by group or others", run.stderr)
        self.assertEqual(self.fake.calls("POST"), [])
        # Owner-only, it loads and scores.
        os.chmod(plugin, 0o644)
        self.fake.answers[("box/m", "c00")] = ("anything", "stop")
        report = self.eval_json("box/m", "--suite", "p")
        self.assertEqual(report["runs"][0]["score"], 1.0)


class NoTextTests(Base):
    def plugin_suite(self, result):
        cases = [case(f"c{i:02d}") for i in range(3)]
        path = self.suite("p", cases, scorer="plugin")
        (path / "score.py").write_text("def score(case, answer):\n    return " + repr(result) + "\n")
        os.chmod(path / "score.py", 0o644)
        for c in cases:
            self.fake.answers[("box/m", c["id"])] = (codes(10), "stop")

    def test_a_plugin_sentence_as_hard_fail_is_refused(self):
        self.plugin_suite({"score": 0.0, "hard_fail": "The patient said: I stopped the pills on Monday",
                           "detail": {}})
        run = self.sb("eval", "box/m", "--suite", "p")
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("hard_fail on c00 must be a short name", run.stderr)
        self.assertEqual(self.fake.quality, [])
        self.assertNotIn("stopped the pills", run.stdout + run.stderr)
        self.assert_released()

    def test_a_plugin_sentence_as_detail_key_is_refused(self):
        self.plugin_suite({"score": 1.0, "hard_fail": None, "detail": {"Dr Smith prescribed 20 mg": 1}})
        run = self.sb("eval", "box/m", "--suite", "p")
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("detail key on c00 is not a short name", run.stderr)
        self.assertEqual(self.fake.quality, [])
        self.assertNotIn("Dr Smith", run.stdout + run.stderr)

    def test_a_plugin_with_name_reasons_and_counts_is_stored(self):
        self.plugin_suite({"score": 0.0, "hard_fail": "too_many", "detail": {"gold_useful": 2}})
        row = self.eval_json("box/m", "--suite", "p")["runs"][0]
        stored = self.fake.quality[-1]
        self.assertEqual(stored["hard_fails"], {"too_many": 6})
        self.assertEqual(stored["case_fail"]["c00"], [{"too_many": 1}, {"too_many": 1}])
        self.assertIsNone(ai_eval.text_problem(stored))
        self.assertEqual(row["detail_totals"], {"gold_useful": 12})

    def test_the_row_check_refuses_text_in_every_shape(self):
        base = {"run_id": "eval_1", "target": "box/m", "suite": "s", "status": "complete",
                "per_case": {"c00": [1.0]}, "hard_fails": {}, "speed": {"input_tokens": 3}}
        self.assertIsNone(ai_eval.text_problem(base))
        for extra in ({"user_prompt": 3}, {"user_prompt": "take the blue pills"},
                      {"\uff50\uff52\uff4f\uff4d\uff50\uff54": 1},
                      {"excerpt": "I stopped the pills on Monday"}, {"note": "ok"},
                      {"notes": ["I stopped", "the pills"]},
                      {"hard_fails": {"The patient said I stopped the pills": 1}},
                      {"per_case": {"I_stopped_the_pills_on_Monday_because_dizzy": [1.0]}},
                      {"per_case": {"i_stopped_the_pills_on_monday": [1.0]}},
                      {"case_fail": {"c00": ["cut"]}},
                      {"detail_totals": {"Dr Smith prescribed 20 mg": 1}},
                      {"speed": {"median_s": "fast"}}):
            self.assertIsNotNone(ai_eval.text_problem(dict(base, **extra)), extra)
        self.assertIsNone(ai_eval.text_problem(dict(base, stop_reason="gateway_error",
                                                    model_file="m" * 200)))
        self.assertIsNotNone(ai_eval.text_problem(dict(base, model_file="m" * 201)))

    def test_a_sentence_as_case_id_is_refused_when_the_suite_loads(self):
        self.suite("s", [case("I_stopped_the_pills_on_Monday_because_dizzy")])
        run = self.sb("eval", "box/m", "--suite", "s")
        self.assertEqual(run.returncode, 2, run.stderr)
        self.assertIn("lower-case letters and digits", run.stderr)
        self.assertEqual(self.fake.calls("POST"), [])


class VerdictTests(Base):
    def run_pair(self, scores_a, scores_b):
        cases = [case(f"c{i:02d}") for i in range(len(scores_a))]
        self.suite("s", cases)
        for c, a, b in zip(cases, scores_a, scores_b):
            self.fake.answers[("lane/a", c["id"])] = (codes(a), "stop")
            self.fake.answers[("lane/b", c["id"])] = (codes(b), "stop")
        return self.eval_json("lane/a", "lane/b", "--suite", "s", "--repeats", "1")

    def test_paired_rule_finds_a_real_lead(self):
        b = [4 + i % 3 for i in range(60)]
        a = [k + 1 for k in b]
        report = self.run_pair(a, b)
        # Each model loaded once.
        loads = [p for m, p, _, _ in self.fake.calls("POST") if p.endswith("/load")]
        self.assertEqual(loads, ["/v1/models/lane/a/load", "/v1/models/lane/b/load"])
        v = report["verdicts"][0]
        self.assertEqual(v["verdict"], "a_beats_b", v)
        self.assertIn("lane/a beats lane/b", v["text"])
        self.assertAlmostEqual(v["difference"], 0.1, places=6)
        for form in (["lane/a", "--compare", "lane/b"], ["--compare", "lane/a", "lane/b"]):
            run = self.sb("eval", *form, "--suite", "s")
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertIn("lane/a beats lane/b", run.stdout)

    def test_paired_rule_ignores_one_lucky_case(self):
        path = self.suite("s", [case("c00")])
        shared = {f"c{i:02d}": [0.4 + 0.1 * (i % 3)] for i in range(30)}
        a = {k: list(v) for k, v in shared.items()}
        b = {k: list(v) for k, v in shared.items()}
        a["c00"], b["c00"] = [1.0], [0.0]
        # Cases only A ran: an unpaired rule would count them.
        a.update({f"x{i:02d}": [1.0] for i in range(15)})
        self.store_row("lane/a", path, a)
        self.store_row("lane/b", path, b)
        v = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s")["verdict"]
        self.assertEqual(v["shared_cases"], 30)
        self.assertNotIn("beats", v["verdict"], v)
        self.assertGreater(v["difference"], 0.02)
        self.assertGreater(v["two_se"], v["difference"])

    def test_too_few_cases_say_add_cases(self):
        path = self.suite("s", [case("c00")])
        rng = random.Random(5)
        a, b = {}, {}
        for i in range(30):
            a[f"c{i:02d}"] = [rng.choice([0.0, 1.0])]
            b[f"c{i:02d}"] = [rng.choice([0.0, 1.0])]
        self.store_row("lane/a", path, a)
        self.store_row("lane/b", path, b)
        v = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s")["verdict"]
        self.assertGreater(v["two_se"], 0.05)
        self.assertEqual(v["verdict"], "cannot_separate", v)
        self.assertIn("add cases", v["text"])
        self.assertNotIn("tie", v["text"])

    def test_a_small_steady_difference_is_a_tie(self):
        path = self.suite("s", [case("c00")])
        a = {f"c{i:02d}": [0.5] for i in range(40)}
        b = {f"c{i:02d}": [0.5] for i in range(40)}
        b["c01"] = [0.4]
        self.store_row("lane/a", path, a)
        self.store_row("lane/b", path, b)
        v = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s")["verdict"]
        self.assertEqual(v["verdict"], "tie", v)

    def test_fewer_than_30_shared_cases_give_no_verdict(self):
        path = self.suite("s", [case("c00")])
        self.store_row("lane/a", path, {f"c{i:02d}": [1.0] for i in range(29)})
        self.store_row("lane/b", path, {f"c{i:02d}": [0.0] for i in range(29)})
        v = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s")["verdict"]
        self.assertEqual(v["verdict"], "no_verdict")
        self.assertIn("29 shared cases", v["text"])

    def test_stale_identity_is_not_current(self):
        path = self.suite("s", [case("c00")])
        good = {f"c{i:02d}": [1.0] for i in range(40)}
        worse = {f"c{i:02d}": [0.0] for i in range(40)}
        self.store_row("lane/b", path, worse)
        for field, value in (("model_file", "file-old"), ("engine_build", "b6"),
                             ("settings_hash", "hash-a-0000")):
            self.fake.quality = [r for r in self.fake.quality if r["target"] != "lane/a"]
            # Same settings revision as now: only the hash tells it is stale.
            self.store_row("lane/a", path, good, **{field: value})
            report = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s")
            self.assertEqual(report["verdict"]["verdict"], "no_verdict", field)
            self.assertIn("no current complete row for lane/a", report["verdict"]["text"])
            report = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s", "--any")
            self.assertEqual(report["verdict"]["verdict"], "a_beats_b", field)
            a = next(r for r in report["rows"] if r["target"] == "lane/a")
            self.assertTrue(a["stale"])
            self.assertEqual(a["stale_because"], [field])
            run = self.sb("eval", "lane/a", "--compare", "lane/b", "--suite", "s", "--any")
            self.assertIn(f"stale: {field}", run.stdout)

    def test_different_suite_versions_never_compare(self):
        path = self.suite("s", [case("c00")])
        self.store_row("lane/a", path, {f"c{i:02d}": [1.0] for i in range(40)})
        self.store_row("lane/b", path, {f"c{i:02d}": [0.0] for i in range(40)}, suite_version="0123456789ab")
        report = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s")
        self.assertEqual(report["verdict"]["verdict"], "no_verdict")
        report = self.eval_json("lane/a", "--compare", "lane/b", "--suite", "s", "--any")
        self.assertEqual(report["verdict"]["verdict"], "no_verdict")
        self.assertIn("different suite versions", report["verdict"]["text"])


class RescoreTests(Base):
    def test_rescore_makes_no_model_call(self):
        cases = [case(f"c{i:02d}") for i in range(5)]
        path = self.suite("s", cases)
        for c in cases:
            self.fake.answers[("box/m", c["id"])] = (codes(6), "stop")
        first = self.eval_json("box/m", "--suite", "s", "--repeats", "1")["runs"][0]
        chats = len(self.fake.chats())
        # A label round: a new labels file and a stricter check.
        (path / "labels.jsonl").write_text(json.dumps({"case": "c00", "label": "useful"}) + "\n")
        lines = [json.loads(line) for line in (path / "cases.jsonl").read_text().splitlines()]
        for c in lines:
            c["expect"]["checks"] = c["expect"]["checks"][:5]
        (path / "cases.jsonl").write_text("".join(json.dumps(c) + "\n" for c in lines))
        self.fake.targets["box/m"]["file"] = "file-new"   # the box moved on; the run did not
        report = self.eval_json("--rescore", first["run_id"])
        self.assertEqual(len(self.fake.chats()), chats)
        self.assertEqual(self.fake.calls("POST", "/v1/hosts"), self.fake.calls("POST", "/v1/hosts")[:1])
        row = report["runs"][0]
        self.assertEqual(row["rescored_from"], first["run_id"])
        self.assertNotEqual(row["run_id"], first["run_id"])
        self.assertNotEqual(row["suite_version"], first["suite_version"])
        self.assertEqual(row["model_file"], "file-m")
        for key in ("engine_build", "settings_hash", "settings_revision", "target"):
            self.assertEqual(row[key], first[key])
        self.assertAlmostEqual(row["score"], 1.0)
        self.assertEqual(self.fake.quality[-1]["rescored_from"], first["run_id"])
        # --no-store needs no gateway at all.
        self.env["SWITCHBACK_AI_URL"] = "http://127.0.0.1:9"
        posted = len(self.fake.quality)
        run = self.sb("eval", "--rescore", first["run_id"], "--no-store")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(self.fake.quality), posted)


class SuiteUnitTests(unittest.TestCase):
    def test_example_suite_accepts_right_answers_and_rejects_wrong_ones(self):
        roots = [str(CLI / "suites")]
        suite = ai_eval.find_suite("example-short-answers", roots)
        self.assertEqual(len(suite.cases), 12)
        right = {
            "invoice-fields": '{"number": "INV-2207", "seller": "Blue Pine Supplies", "total": "16.00"}',
            "invoice-date": '{"date": "2031-03-14"}',
            "invoice-no-extra": '{"pens": 2}',
            "contact-no-extra": '{"name": "Mira Holt", "city": "Lakeview"}',
            "words-ten": "A white tower whose light guides ships home.",
            "one-word": "Blue",
            "sum": "85",
            "product": "108",
            "invoice-check": "Yes, the items sum to 16.00.",
            "sort-alpha": "apple, cherry, mango, pear",
            "sort-numbers": "42, 19, 7, 3",
            "no-guess": "not stated",
        }
        wrong = dict(right, **{
            "invoice-fields": '{"number": "INV-2207", "seller": "Blue Pine", "total": 13.5}',
            "invoice-no-extra": '{"pens": 2, "notebooks": 3}',
            "contact-no-extra": '{"name": "Mira Holt", "city": "Lakeview", "age": 41}',
            "words-ten": "A tall white tower on the rocky coast whose bright light guides every ship home.",
            "sum": "84", "sort-alpha": "pear, mango, cherry, apple", "no-guess": "At 10 am",
            "invoice-check": "No.",
        })
        for c in suite.cases:
            good = suite.score(c, {"text": right[c["id"]], "finish": "stop", "error": None, "usage": {}})
            self.assertEqual(good["score"], 1.0, (c["id"], good))
        for case_id in ("invoice-fields", "invoice-no-extra", "contact-no-extra", "words-ten", "sum",
                        "sort-alpha", "no-guess", "invoice-check"):
            c = next(c for c in suite.cases if c["id"] == case_id)
            bad = suite.score(c, {"text": wrong[case_id], "finish": "stop", "error": None, "usage": {}})
            self.assertLess(bad["score"], 1.0, (case_id, bad))
        self.assertEqual(ai_eval.text_problem({"per_case": {"text01": [1]}}), None)
        self.assertIsNotNone(ai_eval.text_problem({"answer": "x"}))
        self.assertIsNotNone(ai_eval.text_problem({"speed": {"prompt_tokens": 1}}))
        self.assertIsNotNone(ai_eval.text_problem({"note": "n" * 201}))

    def test_suite_version_follows_every_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "s"
            path.mkdir()
            extra = Path(tmp) / "rules.py"
            extra.write_text("RULE = 1\n")
            (path / "suite.json").write_text(json.dumps({"name": "s", "version_inputs": [str(extra)]}))
            (path / "cases.jsonl").write_text(json.dumps(case("c00")) + "\n")
            first = ai_eval.suite_version(path)
            self.assertEqual(len(first), 12)
            self.assertEqual(ai_eval.suite_version(path), first)
            extra.write_text("RULE = 2\n")
            second = ai_eval.suite_version(path)
            self.assertNotEqual(second, first)
            (path / "labels.jsonl").write_text("{}\n")
            self.assertNotEqual(ai_eval.suite_version(path), second)
            extra.unlink()
            with self.assertRaises(ai_eval.EvalError):
                ai_eval.suite_version(path)

    def test_list_needs_no_gateway(self):
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": tempfile.gettempdir(),
               "XDG_CONFIG_HOME": "/nonexistent", "SB_AI_SUITES": "/nonexistent"}
        run = subprocess.run(["zsh", str(SB), "ai", "eval", "--list", "--json"], env=env,
                             capture_output=True, text=True, timeout=60)
        self.assertEqual(run.returncode, 0, run.stderr)
        names = [s["name"] for s in json.loads(run.stdout)["suites"]]
        self.assertIn("example-short-answers", names)


if __name__ == "__main__":
    unittest.main()
