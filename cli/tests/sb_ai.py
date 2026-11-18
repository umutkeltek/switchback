#!/usr/bin/env python3
"""sb ai against a fake gateway on a local port: image and video paths, model
names, status that never wakes, holds, key hygiene, proxy bypass and exit codes.
Every run goes through the real `sb` dispatcher in a child process."""

import json
import os
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
KEY = "sk-test-0f3a9c1d7e5b"
PNG = b"\x89PNG\r\n\x1a\n" + b"\x00\x00\x00\rIHDR" + b"fake-image-body" * 4
MP4 = b"\x00\x00\x00\x18ftypmp42" + b"fake-video-body" * 4


def closed_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def lane(name, state="offline", host="box", **extra):
    row = {"name": name, "host": host, "state": state, "level": "ok", "queue_depth": 0,
           "in_flight": 0, "retries_used": 0, "wake_attempts": 0, "wake_configured": True,
           "poweroff_configured": True, "restart_configured": False, "escalated": False, "detail": ""}
    row.update(extra)
    return row


class Fake:
    """A gateway that answers from `self.routes` and records every request."""

    def __init__(self):
        self.seen = []
        self.client_pid = None
        self.argv_seen = []
        self.lock = threading.Lock()
        self.lanes = [lane("lane-img"), lane("lane-text")]
        self.models = [{"id": "chat-a"}, {"id": "chat-b"}]
        self.workflows = [
            {"id": "mock-image", "kind": "image_generation", "provider": "switchback-mock"},
            {"id": "lane-img/flux", "kind": "image_generation", "provider": "lane-img"},
            {"id": "lane-img/clip", "kind": "video_generation", "provider": "lane-img"},
        ]
        self.workflows_status = 200
        self.job_states = []
        self.holds = None  # None: the gateway has no holds route (bare 404)
        self.routes = {
            ("GET", "/health"): lambda r: (200, b"ok", "text/plain"),
            ("GET", "/v1/workloads/capacity"): lambda r: self.json({"object": "list", "data": self.lanes}),
            ("GET", "/v1/models"): lambda r: self.json({"object": "list", "data": self.models}),
            ("GET", "/v1/workflows"): self.workflows_route,
            ("POST", "/v1/images/generations"): self.image_route,
            ("GET", "/v1/artifacts/art1"): lambda r: (200, PNG, "image/png"),
            ("GET", "/v1/artifacts/vid1"): lambda r: (200, MP4, "video/mp4"),
            ("POST", "/v1/uploads"): lambda r: self.json({"artifact_id": "up1", "kind": "image"}, 201),
            ("POST", "/v1/jobs"): lambda r: self.json({"id": "job-v1", "status": "accepted", "events": []}, 202),
            ("GET", "/v1/jobs/job-v1"): self.job_route,
            ("POST", "/v1/jobs/job-v1/cancel"): lambda r: self.json({"id": "job-v1", "status": "cancelled"}),
        }
        handler = self.handler()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    @staticmethod
    def json(value, status=200):
        return status, json.dumps(value).encode(), "application/json"

    def workflows_route(self, request):
        if self.workflows_status != 200:
            return self.json({"error": {"message": "forbidden", "type": "forbidden"}}, self.workflows_status)
        return self.json({"object": "list", "data": self.workflows})

    def image_route(self, request):
        return self.json({"object": "image.generation", "model": request["json"]["model"],
                          "job": {"id": "job-1234abcd", "kind": "image_generation", "status": "succeeded"},
                          "data": [{"artifact_id": "art1", "media_type": "image/png", "url": "/v1/artifacts/art1"}]})

    def job_route(self, request):
        with self.lock:
            state = self.job_states.pop(0) if len(self.job_states) > 1 else self.job_states[0]
        return self.json(state)

    def holds_route(self, request):
        if self.holds is None:
            return 404, b"", None
        # The shapes of the gateway's holds API (`/v1/hosts/{host}/holds`).
        method, path = request["method"], request["path"]
        host = path.split("/")[3]
        if method == "POST":
            body = request["json"]
            expires_ms = int((time.time() + body["ttl_secs"]) * 1000)
            hold = {"id": f"hold-{len(self.holds) + 1}", "host": host, "reason": body["reason"],
                    "expires_at": "2026-01-01T00:00:00Z", "expires_at_ms": expires_ms,
                    "state": "waking" if body["wake"] else "active", "owner": "agents",
                    "wake_detail": None}
            self.holds.append(hold)
            return self.json(hold, 201)
        if method == "GET":
            return self.json({"object": "list", "host": host, "data": self.holds, "durable": True})
        hold_id = path.rsplit("/", 1)[-1]
        if not any(h["id"] == hold_id for h in self.holds):
            return self.json({"error": {"message": "hold not found", "type": "not_found"}}, 404)
        self.holds = [h for h in self.holds if h["id"] != hold_id]
        return self.json({"id": hold_id, "host": host, "deleted": True})

    def handler(self):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def serve(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                request = {"method": self.command, "path": self.path, "headers": dict(self.headers),
                           "body": body, "json": None}
                try:
                    request["json"] = json.loads(body) if body else None
                except ValueError:
                    pass
                with fake.lock:
                    fake.seen.append(request)
                if fake.client_pid:
                    # Every process on the machine: a child that carried the key would show too.
                    ps = subprocess.run(["ps", "-ww", "-A", "-o", "args="], capture_output=True, text=True)
                    fake.argv_seen.append(ps.stdout)
                route = fake.routes.get((self.command, self.path))
                if route is None and self.path.startswith("/v1/hosts/"):
                    route = fake.holds_route
                if route is None:
                    status, payload, ctype = 404, b"", None  # an unknown route: bare 404, like the server
                else:
                    status, payload, ctype = route(request)
                self.send_response(status)
                if ctype:
                    self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            do_GET = do_POST = do_DELETE = serve

        return Handler


class SbAi(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-ai-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        (self.root / "home").mkdir()
        (self.root / "runtime/config").mkdir(parents=True)
        (self.root / "runtime/manifest.json").write_text("{}")
        (self.root / "xdg/switchback").mkdir(parents=True)
        self.fake = Fake()
        self.addCleanup(self.fake.close)
        dead = f"http://127.0.0.1:{closed_port()}"
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.root / "home"),
            "XDG_CONFIG_HOME": str(self.root / "xdg"),
            "SWITCHBACK_RUNTIME_ROOT": str(self.root / "runtime"),
            "SWITCHBACK_AI_URL": self.fake.url,
            "SWITCHBACK_AI_KEY": KEY,
            "SB_AI_POLL_SECS": "0.05",
            # A dead proxy: a request that honoured it would never reach the fake.
            "HTTP_PROXY": dead, "http_proxy": dead, "HTTPS_PROXY": dead, "https_proxy": dead,
            "ALL_PROXY": dead, "all_proxy": dead, "NO_PROXY": "", "no_proxy": "",
        }

    def sb(self, *args, env=None, timeout=60):
        run = subprocess.run(["zsh", str(SB), "ai", *args], env=env or self.env, cwd=self.root,
                             capture_output=True, text=True, timeout=timeout)
        self.assert_no_key(run.stdout, run.stderr)
        return run

    def assert_no_key(self, *texts):
        for text in texts:
            self.assertNotIn(KEY, text)

    def requests(self, method=None, path=None):
        return [r for r in self.fake.seen
                if (method is None or r["method"] == method) and (path is None or r["path"] == path)]

    def posted(self, path):
        found = self.requests("POST", path)
        self.assertEqual(len(found), 1, f"expected one POST {path}, saw {[r['path'] for r in self.fake.seen]}")
        return found[0]

    # ------------------------------------------------------------ image

    def test_image_saves_the_bytes_and_prints_only_the_path(self):
        out = self.root / "pics/cat.png"
        out.parent.mkdir()
        run = self.sb("image", "a cat", "--model", "flux", "--aspect", "16:9", "--mp", "1.5",
                      "--seed", "7", "-o", str(out))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, f"{out}\n")
        self.assertEqual(out.read_bytes(), PNG)
        body = self.posted("/v1/images/generations")
        self.assertEqual(body["json"], {"model": "lane-img/flux", "prompt": "a cat", "n": 1, "seed": 7,
                                        "metadata": {"aspect_ratio": "16:9", "megapixels": "1.5"}})
        self.assertEqual(body["headers"].get("Authorization"), f"Bearer {KEY}")
        self.assertIn("wakes it", run.stderr)  # the machine is off in the capacity report

    def test_image_default_name_goes_to_the_configured_folder(self):
        folder = self.root / "out"
        (self.root / "xdg/switchback/ai.env").write_text(f"SWITCHBACK_AI_OUT_DIR={folder}\n")
        run = self.sb("image", "a dog", "--model", "lane-img/flux", "--size", "512x512")
        self.assertEqual(run.returncode, 0, run.stderr)
        path = Path(run.stdout.strip())
        self.assertEqual(path.parent, folder)
        self.assertTrue(path.name.startswith("ai-") and path.name.endswith("-flux-1234abcd.png"), path.name)
        self.assertEqual(path.read_bytes(), PNG)
        self.assertEqual(self.posted("/v1/images/generations")["json"]["size"], "512x512")

    def test_unknown_model_lists_the_choices_and_sends_nothing(self):
        run = self.sb("image", "x", "--model", "nope")
        self.assertEqual(run.returncode, 2)
        self.assertIn("unknown image model nope", run.stderr)
        self.assertIn("lane-img/flux", run.stderr)
        self.assertEqual(self.requests("POST"), [])

    def test_ambiguous_model_lists_the_matches(self):
        self.fake.workflows.append({"id": "lane-two/flux", "kind": "image_generation", "provider": "lane-two"})
        run = self.sb("image", "x", "--model", "flux")
        self.assertEqual(run.returncode, 2)
        self.assertIn("matches several image models: lane-img/flux, lane-two/flux", run.stderr)
        self.assertEqual(self.requests("POST"), [])

    def test_video_model_is_not_an_image_model(self):
        run = self.sb("image", "x", "--model", "clip")
        self.assertEqual(run.returncode, 2)
        self.assertIn("is a video model", run.stderr)

    def test_new_style_ids_resolve_from_workflows_and_from_the_model_list(self):
        self.fake.workflows = [{"id": "image/flux", "kind": "image_generation", "provider": "lane-img"}]
        run = self.sb("image", "x", "--model", "flux", "-o", str(self.root / "a.png"))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/images/generations")["json"]["model"], "image/flux")
        # A key that may not list workflows still finds image/<name> in /v1/models.
        self.fake.seen.clear()
        self.fake.workflows_status = 403
        self.fake.models.append({"id": "image/flux"})
        run = self.sb("image", "x", "--model", "flux", "-o", str(self.root / "b.png"))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/images/generations")["json"]["model"], "image/flux")

    def test_gateway_error_exits_1_with_its_message_and_hides_the_key(self):
        self.fake.routes[("POST", "/v1/images/generations")] = lambda r: Fake.json(
            {"error": {"message": f"workflow refused for key {KEY}", "type": "workload_error"}}, 400)
        run = self.sb("image", "x", "--model", "flux")
        self.assertEqual(run.returncode, 1)
        self.assertIn("400: workflow refused for key [key]", run.stderr)

    # ------------------------------------------------------------ status and models

    def test_status_reads_only_capacity(self):
        self.fake.lanes = [
            lane("lane-text", "healthy", host="other", holds_gpu=False, queue_depth=2),
            lane("lane-cd", "idle_countdown", host="other", poweroff_in_secs=420),
        ]
        run = self.sb("status")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual([(r["method"], r["path"]) for r in self.fake.seen], [("GET", "/v1/workloads/capacity")])
        lines = run.stdout.splitlines()
        self.assertEqual(lines[0], "other: on")
        self.assertEqual(lines[1], "  lane-text: idle, level ok, queue 2, GPU with another lane")
        self.assertEqual(lines[2], "  lane-cd: idle, level ok, queue 0, powers off in 7 min")

    def test_status_says_a_held_machine_will_not_power_off(self):
        # The live shape: a held machine's lane stays idle_countdown and its
        # countdown is null. The hold shows on one lane; the other lane of the
        # same machine is held too.
        expires_ms = int((time.time() + 600) * 1000)
        self.fake.lanes = [
            lane("lane-img", "idle_countdown", poweroff_in_secs=None,
                 holds=[{"id": "hold-9", "reason": "render", "owner": "agents", "state": "active",
                         "expires_at": "2026-01-01T00:00:00Z", "expires_at_ms": expires_ms}]),
            lane("lane-text", "idle_countdown", poweroff_in_secs=None),
        ]
        run = self.sb("status", env=dict(self.env, TZ="UTC"))
        self.assertEqual(run.returncode, 0, run.stderr)
        when = time.gmtime(expires_ms / 1000)
        until = time.strftime("%H:%M" if when[:3] == time.gmtime()[:3] else "%b %d %H:%M", when)
        hold = f"1 hold (hold-9 for render by agents until {until}, 10 min left)"
        self.assertEqual(run.stdout.splitlines(), [
            "box: on",
            f"  lane-img: idle, held: will not power off, level ok, queue 0, {hold}",
            f"  lane-text: idle, held: will not power off, level ok, queue 0, {hold}",
        ])
        self.assertNotIn("countdown", run.stdout)
        self.assertNotIn("powers off", run.stdout)

    def test_status_null_countdown_without_a_hold_shows_no_countdown(self):
        self.fake.lanes = [lane("lane-img", "idle_countdown", poweroff_in_secs=None),
                           lane("lane-text", "idle_countdown")]  # no field at all
        run = self.sb("status")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout.splitlines(), ["box: on", "  lane-img: idle, level ok, queue 0",
                                                   "  lane-text: idle, level ok, queue 0"])
        self.assertNotIn("countdown", run.stdout)
        self.assertNotIn("power", run.stdout.split("\n", 1)[1])

    def test_status_json_stays_the_raw_report(self):
        held = lane("lane-img", "idle_countdown", poweroff_in_secs=None,
                    holds=[{"id": "hold-9", "reason": "render", "state": "active", "expires_at_ms": 1}])
        self.fake.lanes = [held]
        run = self.sb("status", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(json.loads(run.stdout)["lanes"], [held])

    def test_status_says_off_and_json_has_machines(self):
        run = self.sb("status", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        report = json.loads(run.stdout)
        self.assertEqual(report["machines"], [{"machine": "box", "power": "off"}])
        self.assertEqual([l["name"] for l in report["lanes"]], ["lane-img", "lane-text"])
        self.assertEqual({r["path"] for r in self.fake.seen}, {"/v1/workloads/capacity"})

    def test_models_groups_by_job(self):
        self.fake.models.append({"id": "video/clip2"})
        run = self.sb("models", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(json.loads(run.stdout), {"text": ["chat-a", "chat-b"],
                                                  "image": ["mock-image", "lane-img/flux"],
                                                  "video": ["lane-img/clip", "video/clip2"]})
        self.assertEqual({r["method"] for r in self.fake.seen}, {"GET"})

    # ------------------------------------------------------------ video

    def video_states(self):
        waiting = lambda **detail: {"event": "waiting_for_lane", "status": "queued",
                                    "created_at_ms": 1, "detail": {"lane": "lane-img", **detail}}
        return [
            {"id": "job-v1", "status": "queued", "events": [waiting(lane_state="offline")]},
            {"id": "job-v1", "status": "queued", "events": [waiting(lane_state="healthy", gpu_group="g",
                                                                    holds_gpu="false")]},
            {"id": "job-v1", "status": "running", "events": []},
            {"id": "job-v1", "status": "succeeded", "events": [],
             "artifacts": [{"artifact_id": "vid1", "kind": "video", "media_type": "video/mp4"}]},
        ]

    def test_video_uploads_starts_polls_and_downloads(self):
        frame = self.root / "first.png"
        frame.write_bytes(PNG)
        self.fake.job_states = self.video_states()
        # The lane is up by the time the job waits for the GPU.
        capacity_calls = []

        def capacity(request):
            capacity_calls.append(1)
            lanes = [lane("lane-img", "offline" if len(capacity_calls) < 3 else "healthy",
                          gpu_group="g", holds_gpu=False)]
            return Fake.json({"data": lanes})
        self.fake.routes[("GET", "/v1/workloads/capacity")] = capacity
        out = self.root / "clip.mp4"
        run = self.sb("video", "waves", "--image", str(frame), "--model", "clip", "--seconds", "2",
                      "--aspect", "16:9", "--seed", "3", "-o", str(out))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, f"{out}\n")
        self.assertEqual(out.read_bytes(), MP4)
        upload = self.posted("/v1/uploads")
        self.assertEqual(upload["body"], PNG)
        self.assertEqual(upload["headers"].get("Content-Type"), "image/png")
        job = self.posted("/v1/jobs")["json"]
        self.assertEqual(job, {"kind": "video_generation", "model": "lane-img/clip", "inputs": {
            "prompt": "waves", "seed": 3, "frames": 48, "size": "864x480", "image": "up1"}})
        self.assertIn("waiting for the machine to wake", run.stderr)
        self.assertIn("waiting for the GPU", run.stderr)
        self.assertIn("running", run.stderr)
        self.assertEqual(self.requests("POST", "/v1/jobs/job-v1/cancel"), [])

    def test_video_failure_shows_the_reason(self):
        frame = self.root / "first.jpg"
        frame.write_bytes(b"\xff\xd8\xff\xe0" + b"jpeg" * 8)
        self.fake.job_states = [{"id": "job-v1", "status": "failed", "events": [
            {"event": "failed", "status": "failed", "created_at_ms": 1, "detail": {"error": "out of memory"}}]}]
        run = self.sb("video", "x", "--image", str(frame), "--model", "lane-img/clip")
        self.assertEqual(run.returncode, 1)
        self.assertIn("job job-v1 failed: out of memory", run.stderr)
        self.assertEqual(self.posted("/v1/uploads")["headers"].get("Content-Type"), "image/jpeg")
        self.assertNotIn("frames", self.posted("/v1/jobs")["json"]["inputs"])

    def test_video_refuses_a_frame_that_is_not_an_image(self):
        frame = self.root / "notes.txt"
        frame.write_text("hello")
        run = self.sb("video", "x", "--image", str(frame), "--model", "clip")
        self.assertEqual(run.returncode, 2)
        self.assertEqual(self.requests("POST"), [])

    def test_interrupt_cancels_the_job_on_the_gateway(self):
        frame = self.root / "first.png"
        frame.write_bytes(PNG)
        self.fake.job_states = [{"id": "job-v1", "status": "running", "events": []}]
        process = subprocess.Popen(["zsh", str(SB), "ai", "video", "x", "--image", str(frame), "--model", "clip"],
                                   env=self.env, cwd=self.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True, start_new_session=True)
        deadline = time.time() + 20
        while time.time() < deadline and not self.requests("GET", "/v1/jobs/job-v1"):
            time.sleep(0.05)
        self.assertTrue(self.requests("GET", "/v1/jobs/job-v1"), "the job was never polled")
        os.killpg(process.pid, signal.SIGINT)  # what Ctrl-C sends to the foreground group
        stdout, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 130, stderr)
        self.assertEqual(stdout, "")
        self.assertEqual(len(self.requests("POST", "/v1/jobs/job-v1/cancel")), 1)
        self.assertIn("cancelled job job-v1", stderr)

    # ------------------------------------------------------------ holds

    def test_hold_and_release_use_the_host_from_capacity(self):
        self.fake.holds = []
        run = self.sb("hold", "1h30m", "render batch", "--wake")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/holds")["json"],
                         {"ttl_secs": 5400, "reason": "render batch", "wake": True})
        self.assertIn("hold hold-1; the gateway wakes it", run.stdout)
        run = self.sb("hold", "30m")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.requests("POST", "/v1/hosts/box/holds")[-1]["json"]["wake"], False)
        # Two holds: a bare release asks which one.
        run = self.sb("release")
        self.assertEqual(run.returncode, 2)
        self.assertIn("hold-1, hold-2", run.stderr)
        run = self.sb("release", "hold-2")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(self.requests("DELETE", "/v1/hosts/box/holds/hold-2")), 1)
        run = self.sb("release", "hold-2")
        self.assertEqual(run.returncode, 1)
        self.assertIn("hold not found", run.stderr)
        run = self.sb("release", "--all")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(self.requests("DELETE", "/v1/hosts/box/holds/hold-1")), 1)
        self.assertEqual(self.fake.holds, [])

    def test_no_holds_route_says_so(self):
        for args in (["hold", "30m"], ["release"], ["release", "hold-1"]):
            run = self.sb(*args)
            self.assertEqual(run.returncode, 1, args)
            self.assertIn("this gateway has no holds yet", run.stderr)

    def test_hold_needs_a_duration_unit_and_one_host(self):
        run = self.sb("hold", "30")
        self.assertEqual(run.returncode, 2)
        self.assertEqual(self.requests("POST"), [])
        self.fake.holds = []
        self.fake.lanes = [lane("lane-a", host="one"), lane("lane-b", host="two")]
        run = self.sb("hold", "10m")
        self.assertEqual(run.returncode, 2)
        self.assertIn("one, two", run.stderr)
        run = self.sb("hold", "10m", "--host", "two")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(self.requests("POST", "/v1/hosts/two/holds")), 1)

    def test_hold_on_a_lane_with_a_machine_of_its_own(self):
        # The full (operator) report leaves `host` out for a lane that has a
        # machine of its own; the holds API names that machine after the lane.
        self.fake.holds = []
        own = lane("lane-solo")
        del own["host"]
        self.fake.lanes = [own]
        run = self.sb("hold", "10m")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(self.requests("POST", "/v1/hosts/lane-solo/holds")), 1)

    # ------------------------------------------------------------ config, key, reachability

    def test_key_from_file_never_in_argv_or_output(self):
        env = dict(self.env)
        del env["SWITCHBACK_AI_KEY"]
        del env["SWITCHBACK_AI_URL"]
        (self.root / "xdg/switchback/ai.env").write_text(
            f"# gateway\nSWITCHBACK_AI_URL={self.fake.url}\nexport SWITCHBACK_AI_KEY=\"{KEY}\"\n")
        process = subprocess.Popen(["zsh", str(SB), "ai", "models"], env=env, cwd=self.root,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.fake.client_pid = process.pid
        stdout, stderr = process.communicate(timeout=30)
        self.assertEqual(process.returncode, 0, stderr)
        self.assert_no_key(stdout, stderr)
        self.assertTrue(self.fake.argv_seen and any("ai_gateway.py" in a for a in self.fake.argv_seen),
                        self.fake.argv_seen)
        for argv in self.fake.argv_seen:
            self.assertNotIn(KEY, argv)
        self.assertEqual({r["headers"].get("Authorization") for r in self.fake.seen}, {f"Bearer {KEY}"})

    def test_environment_wins_over_the_file(self):
        (self.root / "xdg/switchback/ai.env").write_text(
            f"SWITCHBACK_AI_URL=http://127.0.0.1:{closed_port()}\nSWITCHBACK_AI_KEY=from-file\n")
        run = self.sb("status")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.fake.seen[0]["headers"].get("Authorization"), f"Bearer {KEY}")

    def test_unreachable_gateway_exits_3(self):
        env = dict(self.env, SWITCHBACK_AI_URL=f"http://127.0.0.1:{closed_port()}")
        for args in (["status"], ["image", "x", "--model", "flux"], ["hold", "5m"]):
            run = self.sb(*args, env=env)
            self.assertEqual(run.returncode, 3, (args, run.stderr))
            self.assertIn("does not answer", run.stderr)

    def test_missing_address_is_a_usage_error(self):
        env = dict(self.env)
        del env["SWITCHBACK_AI_URL"]
        run = self.sb("status", env=env)
        self.assertEqual(run.returncode, 2)
        self.assertIn("SWITCHBACK_AI_URL", run.stderr)
        self.assertEqual(self.fake.seen, [])

    def test_runs_through_a_symlink_in_another_folder(self):
        # The installed `sb` is a symlink to cli/sb, run from PATH.
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        (bin_dir / "sb").symlink_to(SB)
        env = dict(self.env, PATH=f"{bin_dir}:{self.env['PATH']}")
        run = subprocess.run(["sb", "ai", "status"], env=env, cwd=self.root / "home",
                             capture_output=True, text=True, timeout=60)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("box: off", run.stdout)

    def test_help_and_bad_usage(self):
        run = self.sb("help")
        self.assertEqual(run.returncode, 0)
        self.assertIn("sb ai image", run.stdout)
        self.assertEqual(self.fake.seen, [])
        for args in (["sideways"], ["image"], ["video", "x"], ["image", "x", "--size", "big"]):
            run = self.sb(*args)
            self.assertEqual(run.returncode, 2, args)
        self.assertEqual(self.requests("POST"), [])


if __name__ == "__main__":
    unittest.main()
