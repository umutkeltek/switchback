#!/usr/bin/env python3
"""sb ai against a fake gateway on a local port: image and video paths, model
names, status that never wakes, holds, key hygiene, proxy bypass and exit codes.
Every run goes through the real `sb` dispatcher in a child process."""

import json
import os
import signal
import socket
import struct
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
WAV = b"RIFF\x24\x00\x00\x00WAVEfmt " + bytes(range(256)) + b"\r\n--not-a-boundary\r\n"
SPEECH = b"ID3\x04\x00fake-speech-audio" + bytes(range(256))
DIARIZED = {"task": "transcribe", "duration": 70.0, "text": "hi there all good",
            "segments": [{"speaker": "SPEAKER_00", "start": 0.4, "end": 1.0, "text": " hi"},
                         {"speaker": "SPEAKER_00", "start": 1.2, "end": 2.0, "text": "there "},
                         {"speaker": "SPEAKER_01", "start": 63.2, "end": 65.0, "text": "all good"}]}


def form_parts(request):
    """{name: (filename, content type, bytes)} of a multipart request the fake received."""
    boundary = request["headers"]["Content-Type"].split("boundary=", 1)[1].encode()
    parts = {}
    for chunk in request["body"].split(b"--" + boundary)[1:]:
        if chunk.startswith(b"--"):
            break
        head, _, data = chunk[2:].partition(b"\r\n\r\n")
        head = head.decode()
        name = head.split('name="', 1)[1].split('"', 1)[0]
        filename = head.split('filename="', 1)[1].split('"', 1)[0] if 'filename="' in head else None
        ctype = head.split("Content-Type: ", 1)[1].split("\r\n", 1)[0] if "Content-Type: " in head else None
        parts[name] = (filename, ctype, data[:-2])  # the line break before the next boundary
    return parts


def png(width, height):
    """A PNG header of the given size (the body is not a real image)."""
    return (b"\x89PNG\r\n\x1a\n" + struct.pack(">I", 13) + b"IHDR" + struct.pack(">II", width, height)
            + b"\x08\x02\x00\x00\x00" + b"crc!" + b"fake-image-body" * 4)


def segment(marker, payload):
    return b"\xff" + bytes([marker]) + struct.pack(">H", len(payload) + 2) + payload


def jpeg(width, height, orientation=None, sof=0xC0):
    """A JPEG header: JFIF, optional Exif orientation, a table, then the frame header."""
    parts = [b"\xff\xd8", segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")]
    if orientation is not None:
        tiff = b"MM\x00\x2a" + struct.pack(">I", 8) + struct.pack(">H", 1)
        tiff += struct.pack(">HHI", 0x0112, 3, 1) + struct.pack(">HH", orientation, 0) + struct.pack(">I", 0)
        parts.append(segment(0xE1, b"Exif\x00\x00" + tiff))
    parts.append(segment(0xDB, b"\x00" + b"\x01" * 64))
    parts.append(b"\xff")  # a fill byte before the next marker
    parts.append(segment(sof, b"\x08" + struct.pack(">HH", height, width) + b"\x03" + b"\x01\x22\x00" * 3))
    parts.append(segment(0xDA, b"\x03\x01\x00\x02\x11\x03\x11\x00\x3f\x00") + b"scan" * 8 + b"\xff\xd9")
    return b"".join(parts)


def webp(chunk, width, height):
    """A WebP header with one VP8, VP8L or VP8X chunk."""
    if chunk == b"VP8 ":
        data = b"\x50\x2a\x00" + b"\x9d\x01\x2a" + struct.pack("<HH", width, height)
    elif chunk == b"VP8L":
        data = b"\x2f" + struct.pack("<I", (width - 1) | (height - 1) << 14)
    else:
        data = b"\x00\x00\x00\x00" + (width - 1).to_bytes(3, "little") + (height - 1).to_bytes(3, "little")
    data += b"fake-image-body" * 2
    body = b"WEBP" + chunk + struct.pack("<I", len(data)) + data
    return b"RIFF" + struct.pack("<I", len(body)) + body


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


def chat_plan(delay=0.05, prompt_tokens=1000, completion_tokens=256, timings=None):
    """The chunks of one streamed chat answer: a role, two words of text, then the usage.

    `delay` is the wait before the first token (the time to first token), the
    later gaps are the answer itself. `timings` is what llama.cpp adds to its
    last chunk.
    """
    steps = [(0.0, {"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]}),
             (delay, {"choices": [{"index": 0, "delta": {"content": "The gateway "},
                                    "finish_reason": None}]}),
             (0.05, {"choices": [{"index": 0, "delta": {"content": "keeps the record."},
                                   "finish_reason": None}]})]
    final = {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
             "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens,
                       "total_tokens": prompt_tokens + completion_tokens}}
    if timings:
        final["timings"] = timings
    steps.append((0.05, final))
    return steps


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
            {"id": "segment/sam3", "kind": "workflow_execution", "provider": "lane-img"},
        ]
        self.workflows_status = 200
        self.job_states = []
        self.holds = None  # None: the gateway has no holds route (bare 404)
        # A refused power request: (status, message, reason). None: it works.
        self.power_refusal = None
        self.power_on_status = "waking"  # what `on` answers: "waking" or "up"
        self.already_off = False  # what `off` answers
        self.released_holds = []
        self.jobs_delay = 0.0  # seconds POST /v1/jobs takes to answer
        self.chat_plans = []  # one plan per chat request; the last one is used again
        self.chat_count = 0
        self.chat_fail_at = None  # the request number from which the gateway refuses
        self.chat_status = 500
        self.wake_makes_healthy = False  # a hold with --wake brings the machine up
        self.chat_headers = {}  # extra headers on a chat answer (route, settings)
        # Model settings of text/m: the document, its revision and history.
        self.settings_doc, self.settings_revision, self.settings_history = {}, 0, []
        self.settings_conflict = False  # the next write loses to another writer
        self.settings_fallback = False  # text/m also reaches other/m (a route named like its target)
        self.revert_zero_unknown = False  # a gateway from before revision 0 was revertable
        self.routes = {
            ("GET", "/health"): lambda r: (200, b"ok", "text/plain"),
            ("GET", "/v1/workloads/capacity"): lambda r: self.json({"object": "list", "data": self.lanes}),
            ("GET", "/v1/models"): lambda r: self.json({"object": "list", "data": self.models}),
            ("GET", "/v1/workflows"): self.workflows_route,
            ("POST", "/v1/images/generations"): self.image_route,
            ("GET", "/v1/artifacts/art1"): lambda r: (200, PNG, "image/png"),
            ("GET", "/v1/artifacts/vid1"): lambda r: (200, MP4, "video/mp4"),
            ("POST", "/v1/uploads"): lambda r: self.json({"artifact_id": "up1", "kind": "image"}, 201),
            ("POST", "/v1/jobs"): self.jobs_route,
            ("GET", "/v1/jobs/job-v1"): self.job_route,
            ("POST", "/v1/jobs/job-v1/cancel"): lambda r: self.json({"id": "job-v1", "status": "cancelled"}),
            ("POST", "/v1/audio/transcriptions"): self.transcription_route,
            ("POST", "/v1/audio/speech"): lambda r: (200, SPEECH, "audio/mpeg"),
            ("GET", "/v1/models/text/m/settings"): self.settings_get,
            ("PUT", "/v1/models/text/m/settings"): self.settings_put,
            ("GET", "/v1/models/text/m/settings/history"): lambda r: self.json(
                {"target": "text/m", "revision": self.settings_revision,
                 "history": self.settings_history}),
            ("POST", "/v1/models/text/m/settings/revert"): self.revert_route,
            ("POST", "/v1/models/text/m/load"): self.load_route,
            ("POST", "/v1/models/text/m/unload"): lambda r: self.json(
                {"target": "text/m", "model": "m", "status": "unloaded"}),
            ("GET", "/v1/engines"): lambda r: self.json({"object": "list", "data": [
                {"lane": "text", "kind": "llama_cpp_router", "state": "asleep",
                 "seen_at_ms": 0, "models": [
                     {"id": "m", "status": "loaded", "pending": True, "in_flight": 0,
                      "memory": {"min": {"n_ctx": 4096, "bytes": 2 * 2**30}}}],
                 "variants": [], "last_apply": {"ok": False, "error": "exit 2: pinned"}}]}),
        }
        self.machine_asleep = False  # load without --wake answers 409
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

    def load_route(self, request):
        if self.machine_asleep and not (request["json"] or {}).get("wake"):
            return self.json({"error": {"message": "lane `text` is asleep; send \"wake\": true",
                                        "type": "lane_asleep"}}, 409)
        return self.json({"target": "text/m", "model": "m", "status": "loaded",
                          "memory": {"min": {"n_ctx": 4096, "bytes": 3 * 2**30}}})

    def settings_view(self):
        effective = {"temperature": {"value": None, "source": "engine", "locked": False}}
        for key, value in (self.settings_doc.get("defaults") or {}).items():
            effective[key] = {"value": value, "source": "defaults",
                              "locked": key in self.settings_doc.get("locked", [])}
        return {"target": "text/m", "revision": self.settings_revision, "durable": True,
                "effective": effective, "profiles": self.settings_doc.get("profiles") or {},
                "document": self.settings_doc}

    def revert_route(self, request):
        if request["headers"].get("If-Match") is None:
            return self.json({"error": {"message": "send the revision you edited",
                                        "type": "revision_required"}}, 428)
        if request["headers"].get("If-Match") != str(self.settings_revision):
            return self.json({"error": {"message": "settings changed",
                                        "type": "revision_conflict"}}, 409)
        revision = (request["json"] or {}).get("revision")
        known = {change.get("revision") for change in self.settings_history}
        if revision not in known and (revision != 0 or self.revert_zero_unknown):
            return self.json({"error": {"message": f"no settings revision {revision} in the history",
                                        "type": "not_found"}}, 404)
        if revision == 0:
            self.settings_doc = {}
        self.settings_revision += 1
        return self.json({"target": "text/m", "revision": self.settings_revision,
                          "document": self.settings_doc})

    def settings_get(self, request):
        # With `settings_fallback`, text/m is a public name that also reaches a second target.
        others = [{"target": "other/m", "revision": 0, "durable": True, "effective": {},
                   "profiles": {}, "document": {}}] if self.settings_fallback else []
        return self.json({"model": "text/m", "targets": [self.settings_view()] + others,
                          "revision": self.settings_revision})

    def settings_put(self, request):
        if self.settings_conflict or request["headers"].get("If-Match") != str(self.settings_revision):
            return self.json({"error": {"message": "settings changed: you edited revision 0, "
                                        "the current one is 1", "type": "revision_conflict"}}, 409)
        body = dict(request["json"])
        apply = body.pop("apply", None)
        body.pop("wake", None)
        self.settings_doc = body
        self.settings_revision += 1
        view = self.settings_view()
        if apply:
            view["apply"] = {"mode": apply, "message": f"load settings written ({apply})"}
        return self.json(view)

    def workflows_route(self, request):
        if self.workflows_status != 200:
            return self.json({"error": {"message": "forbidden", "type": "forbidden"}}, self.workflows_status)
        return self.json({"object": "list", "data": self.workflows})

    def image_route(self, request):
        return self.json({"object": "image.generation", "model": request["json"]["model"],
                          "job": {"id": "job-1234abcd", "kind": "image_generation", "status": "succeeded"},
                          "data": [{"artifact_id": "art1", "media_type": "image/png", "url": "/v1/artifacts/art1"}]})

    def transcription_route(self, request):
        """The gateway's `/v1/audio/transcriptions`: the engine's answer per format."""
        fmt = form_parts(request).get("response_format", (None, None, b"json"))[2].decode()
        if fmt == "diarized_json":
            return self.json(DIARIZED)
        if fmt == "srt":
            return 200, b"1\n00:00:00,000 --> 00:00:01,000\nhello world\n", "text/plain; charset=utf-8"
        if fmt == "json":
            return self.json({"text": "hello world"})
        return 200, "hello world, grüße".encode(), "text/plain; charset=utf-8"

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
            if self.wake_makes_healthy:
                with self.lock:
                    self.lanes = [dict(row, state="healthy") for row in self.lanes]
            return self.json(hold, 201)
        if method == "GET":
            return self.json({"object": "list", "host": host, "data": self.holds, "durable": True})
        hold_id = path.rsplit("/", 1)[-1]
        if not any(h["id"] == hold_id for h in self.holds):
            return self.json({"error": {"message": "hold not found", "type": "not_found"}}, 404)
        self.holds = [h for h in self.holds if h["id"] != hold_id]
        return self.json({"id": hold_id, "host": host, "deleted": True})

    def jobs_route(self, request):
        """`POST /v1/jobs`: the job starts (after `jobs_delay`)."""
        time.sleep(self.jobs_delay)
        return self.json({"id": "job-v1", "status": "accepted", "events": []}, 202)

    def power_route(self, request):
        """`POST /v1/hosts/{host}/power`: the gateway's power endpoint."""
        if self.power_refusal:
            status, message, reason = self.power_refusal
            return self.json({"error": {"message": message, "type": "power_refused",
                                        "reason": reason}}, status)
        host = request["path"].split("/")[3]
        if request["json"].get("action") == "on":
            if self.power_on_status == "up":
                return self.json({"host": host, "action": "on", "status": "up", "lanes": 1})
            return self.json({"host": host, "action": "on", "status": "waking", "lanes": 1}, 202)
        return self.json({"host": host, "action": request["json"].get("action"),
                          "powered_off": True, "already_off": self.already_off, "lanes": 1,
                          "released_holds": self.released_holds})

    def chat_plan_for(self):
        """The plan for the next chat request, or None when the gateway refuses it."""
        with self.lock:
            self.chat_count += 1
            count = self.chat_count
        if self.chat_fail_at is not None and count >= self.chat_fail_at:
            return None
        plans = self.chat_plans or [chat_plan()]
        return plans[min(count - 1, len(plans) - 1)]

    def stream_answer(self, handler):
        """Answer a chat request with server-sent events, one flush at a time."""
        plan = self.chat_plan_for()
        if plan is None:
            payload = json.dumps({"error": {"message": "the lane refused the request",
                                            "type": "workload_error"}}).encode()
            handler.send_response(self.chat_status)
            handler.send_header("Content-Type", "application/json")
            handler.send_header("Content-Length", str(len(payload)))
            handler.end_headers()
            handler.wfile.write(payload)
            return
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream")
        for name, value in self.chat_headers.items():
            handler.send_header(name, value)
        handler.end_headers()
        try:
            for delay, chunk in plan:
                if delay:
                    time.sleep(delay)
                handler.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                handler.wfile.flush()
            handler.wfile.write(b"data: [DONE]\n\n")
            handler.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # the client hung up (an interrupted run)

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
                if self.command == "POST" and self.path == "/v1/chat/completions":
                    fake.stream_answer(self)
                    return
                route = fake.routes.get((self.command, self.path))
                if route is None and self.path.startswith("/v1/hosts/") and self.path.endswith("/power"):
                    route = fake.power_route
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

            do_GET = do_POST = do_DELETE = do_PUT = do_PATCH = serve

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

    def test_an_unlisted_full_id_goes_to_the_gateway(self):
        """An old name kept as an alias resolves at the gateway but is never
        listed; the CLI sends a full id of the right group and lets the
        gateway decide."""
        run = self.sb("image", "a dog", "--model", "image/old-name")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/images/generations")["json"]["model"], "image/old-name")

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
                                                  "video": ["lane-img/clip", "video/clip2"],
                                                  "segment": ["segment/sam3"],
                                                  "audio": []})
        self.assertEqual({r["method"] for r in self.fake.seen}, {"GET"})

    def test_models_lists_each_text_model_once(self):
        # A real gateway lists each route (`chat-a`), its provider's own ids
        # (`lane-txt/chat-a`, `lane-txt/engine-x`) and the provider (`lane-txt`).
        # Only route names show; an id whose provider is not listed stays.
        self.fake.models.extend([{"id": "lane-txt/chat-a"}, {"id": "lane-txt/chat-b"},
                                 {"id": "lane-txt"}, {"id": "lane-txt/engine-x"}, {"id": "other/only-direct"},
                                 {"id": "lane-img"}])  # the image workflows' provider
        run = self.sb("models", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(json.loads(run.stdout)["text"], ["chat-a", "chat-b", "other/only-direct"])
        run = self.sb("models", "--all", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("lane-txt/engine-x", json.loads(run.stdout)["text"])
        run = self.sb("models")
        self.assertNotIn("lane-txt/chat-a", run.stdout)
        self.assertNotRegex(run.stdout, r"(?m)^  lane-txt$")

    def test_models_lists_audio_models_in_their_own_group(self):
        self.fake.models.extend([{"id": "transcribe", "kind": "audio_transcription"},
                                 {"id": "stt/whisper", "kind": "audio_transcription"},
                                 {"id": "stt", "kind": "audio_transcription"},
                                 {"id": "speak", "kind": "audio_speech"}])
        run = self.sb("models", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        groups = json.loads(run.stdout)
        # Route names, like text: the provider's own ids show with --all.
        self.assertEqual(groups["audio"], ["transcribe", "speak"])
        self.assertEqual(groups["text"], ["chat-a", "chat-b"])
        run = self.sb("models", "--all", "--json")
        self.assertEqual(json.loads(run.stdout)["audio"], ["transcribe", "stt/whisper", "stt", "speak"])
        run = self.sb("models")
        self.assertIn("audio:\n  transcribe\n  speak (speech)\n", run.stdout)

    # ------------------------------------------------------------ audio

    def wav(self, name="talk.wav", data=WAV):
        path = self.root / name
        path.write_bytes(data)
        return path

    def test_transcribe_prints_the_text_and_sends_the_file_unchanged(self):
        run = self.sb("transcribe", str(self.wav()), "--language", "de")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, "hello world, grüße\n")
        request = self.posted("/v1/audio/transcriptions")
        self.assertEqual(request["headers"].get("Authorization"), f"Bearer {KEY}")
        self.assertTrue(request["headers"]["Content-Type"].startswith("multipart/form-data; boundary="))
        parts = form_parts(request)
        self.assertEqual(parts["model"][2], b"transcribe")  # the operator's route, by default
        self.assertEqual(parts["response_format"][2], b"text")
        self.assertEqual(parts["language"][2], b"de")
        self.assertEqual(parts["file"], ("talk.wav", "audio/wav", WAV))
        self.assertIn("transcribing talk.wav", run.stderr)

    def test_transcribe_srt_goes_to_the_output_file(self):
        out = self.root / "talk.srt"
        run = self.sb("transcribe", str(self.wav()), "--format", "srt", "--model", "stt/whisper",
                      "-o", str(out))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, f"{out}\n")
        self.assertEqual(out.read_text(), "1\n00:00:00,000 --> 00:00:01,000\nhello world\n")
        parts = form_parts(self.posted("/v1/audio/transcriptions"))
        self.assertEqual(parts["model"][2], b"stt/whisper")
        self.assertEqual(parts["response_format"][2], b"srt")

    def test_transcribe_speakers_prints_one_line_per_turn(self):
        run = self.sb("transcribe", str(self.wav()), "--speakers")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, "[00:00:00] SPEAKER_00: hi there\n[00:01:03] SPEAKER_01: all good\n")
        parts = form_parts(self.posted("/v1/audio/transcriptions"))
        self.assertEqual(parts["model"][2], b"diarize")
        self.assertEqual(parts["response_format"][2], b"diarized_json")
        # --format json keeps the raw answer.
        self.fake.seen.clear()
        run = self.sb("transcribe", str(self.wav()), "--speakers", "--format", "json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(json.loads(run.stdout), DIARIZED)

    def fake_ffmpeg(self, encoders=" A..... libopus              libopus Opus\n"):
        """An ffmpeg on the PATH that lists `encoders` and writes its own
        arguments into the output file (the last argument)."""
        bin_dir = self.root / "bin"
        bin_dir.mkdir(exist_ok=True)
        ffmpeg = bin_dir / "ffmpeg"
        ffmpeg.write_text('#!/bin/sh\ncase "$*" in *-encoders*) printf %s "$ENCODERS"; exit 0;; esac\n'
                          'for last; do :; done\nprintf "FROM-ffmpeg:%s" "$*" > "$last"\n')
        ffmpeg.chmod(0o755)
        return ffmpeg, dict(self.env, PATH=f"{bin_dir}:{self.env['PATH']}", ENCODERS=encoders)

    def test_transcribe_turns_other_files_into_small_opus_with_ffmpeg(self):
        ffmpeg, env = self.fake_ffmpeg()
        source = self.wav("meeting.mkv", b"matroska")
        run = self.sb("transcribe", str(source), env=env)
        self.assertEqual(run.returncode, 0, run.stderr)
        filename, ctype, data = form_parts(self.posted("/v1/audio/transcriptions"))["file"]
        self.assertEqual((filename, ctype), ("meeting.ogg", "audio/ogg"))
        self.assertTrue(data.startswith(b"FROM-ffmpeg:"), data)
        # Mono 16 kHz Opus at 32 kbps: an hour is about 14 MiB, not 110 MiB of wav.
        self.assertIn(b"-ac 1 -ar 16000 -c:a libopus -b:a 32k -f ogg", data)
        # The input is always a plain file, never a protocol like concat:.
        self.assertIn(f"-i file:{source}".encode(), data)
        self.assertIn("to 16 kHz mono Ogg/Opus", run.stderr)
        # A file engines read as it is never goes through ffmpeg.
        self.fake.seen.clear()
        run = self.sb("transcribe", str(self.wav("talk.mp3", b"ID3mp3")), env=env)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(form_parts(self.posted("/v1/audio/transcriptions"))["file"],
                         ("talk.mp3", "audio/mpeg", b"ID3mp3"))
        # ffmpeg that cannot read the file: a usage error, nothing sent.
        self.fake.seen.clear()
        ffmpeg.write_text('#!/bin/sh\ncase "$*" in *-encoders*) exit 0;; esac\n'
                          "echo 'Invalid data found when processing input' >&2\nexit 1\n")
        run = self.sb("transcribe", str(self.wav("broken.mkv", b"x")), env=env)
        self.assertEqual(run.returncode, 2)
        self.assertIn("ffmpeg cannot read", run.stderr)
        self.assertIn("Invalid data found", run.stderr)
        self.assertEqual(self.requests("POST"), [])

    def test_transcribe_falls_back_to_flac_without_libopus(self):
        _, env = self.fake_ffmpeg(encoders=" A..... flac                 FLAC\n")
        source = self.wav("concat:meeting.mkv", b"matroska")
        run = self.sb("transcribe", str(source), env=env)
        self.assertEqual(run.returncode, 0, run.stderr)
        filename, ctype, data = form_parts(self.posted("/v1/audio/transcriptions"))["file"]
        self.assertEqual((filename, ctype), ("concat:meeting.flac", "audio/flac"))
        self.assertIn(b"-ac 1 -ar 16000 -c:a flac -f flac", data)
        self.assertIn(f"-i file:{source}".encode(), data)

    def test_transcribe_error_paths(self):
        run = self.sb("transcribe", str(self.root / "missing.wav"))
        self.assertEqual(run.returncode, 2)
        self.assertIn("no such file", run.stderr)
        run = self.sb("transcribe", str(self.wav()), "--speakers", "--format", "srt")
        self.assertEqual(run.returncode, 2)
        self.assertEqual(self.requests("POST"), [])
        self.fake.routes[("POST", "/v1/audio/transcriptions")] = lambda r: Fake.json(
            {"error": {"message": "the upload is larger than 26214400 bytes", "type": "audio_too_large"}}, 413)
        run = self.sb("transcribe", str(self.wav()))
        self.assertEqual(run.returncode, 1)
        self.assertIn("too large for the gateway: the upload is larger than 26214400 bytes", run.stderr)
        self.fake.routes[("POST", "/v1/audio/transcriptions")] = lambda r: Fake.json(
            {"error": {"message": f"no eligible target for key {KEY}", "type": "invalid_request_error"}}, 400)
        run = self.sb("transcribe", str(self.wav()))
        self.assertEqual(run.returncode, 1)
        self.assertIn("the transcription failed: the gateway said 400: no eligible target for key [key]", run.stderr)
        env = dict(self.env, SWITCHBACK_AI_URL=f"http://127.0.0.1:{closed_port()}")
        run = self.sb("transcribe", str(self.wav()), env=env)
        self.assertEqual(run.returncode, 3)
        self.assertIn("does not answer", run.stderr)

    def test_speak_saves_the_audio_in_the_format_of_the_extension(self):
        # What a real gateway lists for one speech route: the route, its
        # target and its provider, all marked as speech.
        self.fake.models.extend([{"id": "speak", "kind": "audio_speech"},
                                 {"id": "tts/kokoro", "kind": "audio_speech"},
                                 {"id": "tts", "kind": "audio_speech"},
                                 {"id": "transcribe", "kind": "audio_transcription"},
                                 {"id": "stt/whisper", "kind": "audio_transcription"},
                                 {"id": "stt", "kind": "audio_transcription"}])
        out = self.root / "hello.mp3"
        run = self.sb("speak", "Hallo Welt", "--voice", "af_heart", "-o", str(out))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, f"{out}\n")
        self.assertEqual(out.read_bytes(), SPEECH)
        self.assertEqual(self.posted("/v1/audio/speech")["json"],
                         {"model": "speak", "input": "Hallo Welt", "voice": "af_heart",
                          "response_format": "mp3"})
        # The configured default wins over the gateway's list.
        self.fake.seen.clear()
        env = dict(self.env, SWITCHBACK_AI_SPEECH_MODEL="tts/kokoro")
        run = self.sb("speak", "hi", "-o", str(self.root / "hi.ogg"), env=env)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/audio/speech")["json"],
                         {"model": "tts/kokoro", "input": "hi", "response_format": "opus"})

    def test_speak_error_paths(self):
        run = self.sb("speak", "hi")  # no -o
        self.assertEqual(run.returncode, 2)
        for name in ("a.m4a", "a.out", "noextension"):  # no speech format matches
            run = self.sb("speak", "hi", "--model", "speak", "-o", str(self.root / name))
            self.assertEqual(run.returncode, 2, name)
            self.assertIn("-o must end in one of .mp3, .wav, .opus, .ogg, .aac, .flac, .pcm", run.stderr)
        self.assertEqual(self.requests("POST"), [])
        run = self.sb("speak", "hi", "-o", str(self.root / "a.wav"))  # the gateway offers no speech model
        self.assertEqual(run.returncode, 2)
        self.assertIn("choose a speech model with --model (the gateway offers: none)", run.stderr)
        self.assertEqual(self.requests("POST"), [])
        self.fake.routes[("POST", "/v1/audio/speech")] = lambda r: Fake.json(
            {"error": {"message": "audio output required but target does not support it",
                       "type": "invalid_request_error"}}, 400)
        run = self.sb("speak", "hi", "--model", "chat-a", "-o", str(self.root / "a.wav"))
        self.assertEqual(run.returncode, 1)
        self.assertIn("the speech failed: the gateway said 400: audio output required", run.stderr)
        self.assertFalse((self.root / "a.wav").exists())

    def test_audio_help(self):
        run = self.sb("help")
        self.assertIn("sb ai transcribe FILE", run.stdout)
        self.assertIn("sb ai speak \"TEXT\"", run.stdout)
        self.assertEqual(self.fake.seen, [])

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

    def video_size(self, frame_bytes, name, *flags):
        """Run a video with this first frame; return the run and the job's inputs."""
        frame = self.root / name
        frame.write_bytes(frame_bytes)
        self.fake.seen.clear()
        self.fake.job_states = [{"id": "job-v1", "status": "succeeded", "events": [],
                                 "artifacts": [{"artifact_id": "vid1", "kind": "video", "media_type": "video/mp4"}]}]
        run = self.sb("video", "x", "--image", str(frame), "--model", "clip", *flags,
                      "-o", str(self.root / "clip.mp4"))
        self.assertEqual(run.returncode, 0, run.stderr)
        return run, self.posted("/v1/jobs")["json"]["inputs"]

    def test_video_keeps_the_shape_of_a_wide_png(self):
        run, inputs = self.video_size(png(1920, 1080), "wide.png")
        self.assertEqual(inputs["size"], "864x480")
        self.assertIn("first frame is 1920x1080; sending size 864x480", run.stderr)

    def test_video_keeps_the_shape_of_a_tall_jpeg(self):
        run, inputs = self.video_size(jpeg(1080, 1920), "tall.jpg")
        self.assertEqual(inputs["size"], "480x864")
        # A progressive JPEG (SOF2) is read the same way.
        run, inputs = self.video_size(jpeg(1080, 1920, sof=0xC2), "tall-progressive.jpg")
        self.assertEqual(inputs["size"], "480x864")
        # A phone photo stored wide with Exif orientation 6 shows tall.
        run, inputs = self.video_size(jpeg(1920, 1080, orientation=6), "turned.jpg")
        self.assertEqual(inputs["size"], "480x864")

    def test_video_keeps_the_shape_of_a_webp(self):
        for chunk, width, height, size in ((b"VP8 ", 1920, 1080, "864x480"),
                                           (b"VP8L", 1080, 1920, "480x864"),
                                           (b"VP8X", 1024, 768, "736x576")):
            with self.subTest(chunk=chunk):
                run, inputs = self.video_size(webp(chunk, width, height), "frame.webp")
                self.assertEqual(inputs["size"], size)
                self.assertIn(f"first frame is {width}x{height}", run.stderr)

    def test_video_explicit_shape_wins_over_the_frame(self):
        wide = png(1920, 1080)
        run, inputs = self.video_size(wide, "wide.png", "--aspect", "1:1")
        self.assertEqual(inputs["size"], "640x640")
        self.assertNotIn("first frame is", run.stderr)
        run, inputs = self.video_size(wide, "wide.png", "--aspect", "9:16")
        self.assertEqual(inputs["size"], "480x864")
        run, inputs = self.video_size(wide, "wide.png", "--size", "512x512")
        self.assertEqual(inputs["size"], "512x512")
        run, inputs = self.video_size(wide, "wide.png", "--mp", "0.4")  # --mp alone keeps the frame's shape
        self.assertEqual(inputs["size"], "864x480")
        self.assertIn("first frame is 1920x1080", run.stderr)
        run, inputs = self.video_size(wide, "wide.png", "--mp", "0.9")  # ...at the asked size
        width, height = (int(x) for x in inputs["size"].split("x"))
        self.assertGreater(width, height)
        self.assertGreater(width * height, 864 * 480)
        run, inputs = self.video_size(b"\xff\xd8\xff\xe0" + b"jpeg" * 8, "broken.jpg", "--mp", "0.4")
        self.assertEqual(inputs["size"], "640x640")  # unreadable frame + --mp: square at that size

    def test_video_unreadable_header_sends_no_size_and_says_so(self):
        for frame_bytes, name in ((b"\xff\xd8\xff\xe0" + b"jpeg" * 8, "broken.jpg"),
                                  (b"\x89PNG\r\n\x1a\n" + b"short", "short.png"),
                                  (jpeg(0, 0), "zero.jpg"),
                                  (b"RIFF\x10\x00\x00\x00WEBPVP8Z" + b"\x00" * 20, "odd.webp")):
            with self.subTest(name=name):
                run, inputs = self.video_size(frame_bytes, name)
                self.assertNotIn("size", inputs)
                self.assertIn("cannot read the first frame's width and height; sending no size", run.stderr)

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

    def test_bare_hold_means_the_only_machine_that_sleeps(self):
        # An always-on machine (no wake, no power-off) beside the one that sleeps:
        # a bare hold, release and bench of a route name mean the sleeping one.
        self.fake.holds = []
        self.fake.lanes = [lane("lane-text", "healthy", host="box"),
                           lane("mac", "healthy", host="desk", wake_configured=False,
                                poweroff_configured=False)]
        run = self.sb("hold", "10m", "--wake")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(self.requests("POST", "/v1/hosts/box/holds")), 1)
        self.assertEqual(self.requests("POST", "/v1/hosts/desk/holds"), [])
        # Two sleeping machines stay ambiguous.
        self.fake.lanes = [lane("lane-a", host="one"), lane("lane-b", host="two"),
                           lane("mac", host="desk", wake_configured=False, poweroff_configured=False)]
        run = self.sb("hold", "10m")
        self.assertEqual(run.returncode, 2)
        self.assertIn("desk, one, two", run.stderr)

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

    # ------------------------------------------------------------ power

    def test_on_takes_a_hold_with_a_wake_and_says_how_to_let_it_go(self):
        self.fake.holds = []
        run = self.sb("on")
        self.assertEqual(run.returncode, 0, run.stderr)
        # One hour by default, and the gateway wakes the machine.
        self.assertEqual(self.posted("/v1/hosts/box/holds")["json"],
                         {"ttl_secs": 3600, "reason": "sb ai on", "wake": True})
        self.assertIn("hold hold-1", run.stdout)
        self.assertIn("let it go: sb ai release hold-1", run.stdout)

    def test_on_takes_a_duration_and_a_reason(self):
        self.fake.holds = []
        run = self.sb("on", "2h", "render batch")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/holds")["json"],
                         {"ttl_secs": 7200, "reason": "render batch", "wake": True})

    def test_on_says_when_the_machine_is_already_up(self):
        self.fake.holds = []
        self.fake.power_on_status = "up"
        run = self.sb("on")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/power")["json"], {"action": "on"})
        # The machine is up: the hold keeps it on, and wakes nothing.
        self.assertEqual(self.posted("/v1/hosts/box/holds")["json"],
                         {"ttl_secs": 3600, "reason": "sb ai on", "wake": False})
        self.assertIn("box is already up", run.stdout)

    def test_on_says_the_machine_is_waking(self):
        self.fake.holds = []
        run = self.sb("on")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("box is waking", run.stdout)

    def test_on_with_a_key_that_may_not_power_still_holds_and_wakes(self):
        self.fake.holds = []
        self.fake.power_refusal = (403, "operator keys only", "")
        run = self.sb("on")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/holds")["json"],
                         {"ttl_secs": 3600, "reason": "sb ai on", "wake": True})

    def test_on_needs_a_duration_unit(self):
        run = self.sb("on", "30")
        self.assertEqual(run.returncode, 2)
        self.assertIn("DURATION wants a unit", run.stderr)
        self.assertEqual(self.requests("POST"), [])

    def test_off_asks_the_gateway_to_power_the_machine_off(self):
        run = self.sb("off")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/power")["json"], {"action": "off"})
        self.assertIn("box is powering off", run.stdout)

    def test_off_says_when_the_machine_is_already_off(self):
        self.fake.already_off = True
        run = self.sb("off")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("box is already off", run.stdout)
        self.assertNotIn("powering off", run.stdout)

    def test_off_names_the_released_holds(self):
        self.fake.released_holds = ["hold-1"]
        run = self.sb("off", "--release-holds")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("released hold hold-1 on box", run.stdout)
        self.assertIn("box is powering off", run.stdout)

    def test_off_releases_the_holds_when_asked(self):
        run = self.sb("off", "--release-holds")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/power")["json"],
                         {"action": "off", "release_holds": True})

    def test_off_prints_the_reason_when_the_gateway_refuses(self):
        self.fake.power_refusal = (409, "1 hold active on this machine (hold-1)", "hold_active")
        run = self.sb("off")
        self.assertEqual(run.returncode, 1)
        self.assertIn("hold_active", run.stderr)
        self.assertIn("hold-1", run.stderr)

    def test_off_needs_one_machine(self):
        self.fake.lanes = [lane("lane-a", host="one"), lane("lane-b", host="two")]
        run = self.sb("off")
        self.assertEqual(run.returncode, 2)
        self.assertIn("one, two", run.stderr)
        self.assertEqual(self.requests("POST", "/v1/hosts/one/power"), [])

    def test_off_with_an_unreachable_gateway_exits_3(self):
        dead = f"http://127.0.0.1:{closed_port()}"
        env = dict(self.env, SWITCHBACK_AI_URL=dead)
        run = self.sb("off", env=env)
        self.assertEqual(run.returncode, 3)

    # ------------------------------------------------------------ segment

    def segment_states(self):
        return [{"id": "job-v1", "status": "succeeded", "events": [],
                 "artifacts": [{"artifact_id": "art1", "kind": "image",
                                "media_type": "image/png"}]}]

    def test_segment_uploads_the_image_and_saves_the_mask(self):
        picture = self.root / "cat.png"
        picture.write_bytes(PNG)
        self.fake.job_states = self.segment_states()
        mask = self.root / "mask.png"
        run = self.sb("segment", str(picture), "-o", str(mask))
        self.assertEqual(run.returncode, 0, run.stderr)
        # The image went up as an upload, and its id is the job's image input.
        upload = self.posted("/v1/uploads")
        self.assertEqual(upload["headers"].get("Content-Type"), "image/png")
        self.assertEqual(upload["body"], PNG)
        job = self.posted("/v1/jobs")
        self.assertEqual(job["json"]["kind"], "workflow_execution")
        self.assertEqual(job["json"]["model"], "segment/sam3")
        self.assertEqual(job["json"]["inputs"], {"image": "up1"})
        # The mask came back as an artifact and was saved.
        self.assertEqual(run.stdout, f"{mask}\n")
        self.assertEqual(mask.read_bytes(), PNG)

    def test_segment_sends_the_text_prompt(self):
        picture = self.root / "cat.png"
        picture.write_bytes(PNG)
        self.fake.job_states = self.segment_states()
        run = self.sb("segment", str(picture), "the cat", "-o", str(self.root / "m.png"))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/jobs")["json"]["inputs"],
                         {"prompt": "the cat", "image": "up1"})

    def test_segment_needs_a_model_the_gateway_offers(self):
        picture = self.root / "cat.png"
        picture.write_bytes(PNG)
        run = self.sb("segment", str(picture), "--model", "nope")
        self.assertEqual(run.returncode, 2)
        self.assertIn("unknown segment model nope", run.stderr)
        self.assertEqual(self.requests("POST"), [])

    def test_segment_refuses_a_file_that_is_not_an_image(self):
        picture = self.root / "cat.txt"
        picture.write_bytes(b"not an image")
        run = self.sb("segment", str(picture))
        self.assertEqual(run.returncode, 2)
        self.assertIn("must be a PNG, JPEG or WebP file", run.stderr)
        self.assertEqual(self.requests("POST"), [])

    def test_segment_refuses_a_file_it_cannot_read(self):
        run = self.sb("segment", str(self.root / "missing.png"))
        self.assertEqual(run.returncode, 2)
        self.assertIn("cannot read", run.stderr)

    def test_segment_failure_shows_the_reason(self):
        picture = self.root / "cat.png"
        picture.write_bytes(PNG)
        self.fake.job_states = [{"id": "job-v1", "status": "failed", "events": [
            {"event": "failed", "status": "failed", "created_at_ms": 1,
             "detail": {"error": "the lane refused the job"}}]}]
        run = self.sb("segment", str(picture))
        self.assertEqual(run.returncode, 1)
        self.assertIn("the lane refused the job", run.stderr)

    def test_segment_interrupt_cancels_the_job_on_the_gateway(self):
        picture = self.root / "cat.png"
        picture.write_bytes(PNG)
        self.fake.job_states = [{"id": "job-v1", "status": "running", "events": []}]
        process = subprocess.Popen(["zsh", str(SB), "ai", "segment", str(picture)],
                                   env=self.env, cwd=self.root, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, start_new_session=True)
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

    def test_segment_interrupt_while_the_job_is_sent_says_it_may_still_run(self):
        picture = self.root / "cat.png"
        picture.write_bytes(PNG)
        self.fake.jobs_delay = 5.0  # the answer to POST /v1/jobs is slow
        process = subprocess.Popen(["zsh", str(SB), "ai", "segment", str(picture)],
                                   env=self.env, cwd=self.root, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, start_new_session=True)
        deadline = time.time() + 20
        while time.time() < deadline and not self.requests("POST", "/v1/jobs"):
            time.sleep(0.05)
        self.assertTrue(self.requests("POST", "/v1/jobs"), "the job was never sent")
        os.killpg(process.pid, signal.SIGINT)
        stdout, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 130, stderr)
        self.assertEqual(stdout, "")
        self.assertIn("may still run", stderr)
        self.assertIn("/v1/jobs/ID/cancel", stderr)
        self.assertNotIn("before the job started", stderr)

    # ------------------------------------------------------------ bench

    def healthy(self):
        """The machine is up: its lanes serve."""
        self.fake.lanes = [lane("lane-img", "healthy"), lane("lane-text", "healthy")]

    def bench(self, *args):
        return self.sb("bench", *args)

    def assert_close(self, got, want, delta=0.5):
        self.assertAlmostEqual(got, want, delta=max(delta, abs(want) * 1e-6))

    def test_bench_measures_the_client_math(self):
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.1)]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "1", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        row = json.loads(run.stdout)["results"][0]
        self.assertEqual(row["source"], "client")
        self.assertEqual(row["prompt_tokens"], 1000)
        self.assertEqual(row["completion_tokens"], 256)
        self.assertGreater(row["ttft_ms"], 50)
        self.assertGreater(row["total_s"], row["ttft_ms"] / 1000)
        # the prompt speed is the prompt over the time to the first token
        self.assert_close(row["prompt_tok_s"], row["prompt_tokens"] / (row["ttft_ms"] / 1000))
        # and the decode speed is the answer over the time after the first token
        self.assert_close(row["decode_tok_s"],
                          row["completion_tokens"] / (row["total_s"] - row["ttft_ms"] / 1000))

    def test_bench_sends_a_new_prompt_each_run_unless_keep_cache(self):
        # The same prompt twice lets the engine skip reading it: warm numbers
        # would show the cache, not the reading speed.
        def prompts():
            return [r["json"]["messages"][0]["content"]
                    for r in self.requests("POST", "/v1/chat/completions")]
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.01) for _ in range(4)]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "2", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(set(prompts())), 2)
        self.fake.seen.clear()
        run = self.bench("chat-a", "--workload", "chat", "--runs", "2", "--keep-cache", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(set(prompts())), 1)

    def test_bench_counts_a_reasoning_token_as_the_first_token(self):
        # A thinking model streams reasoning first; the answer comes 0.4 s later.
        self.healthy()
        plan = chat_plan(delay=0.05)
        plan.insert(1, (0.05, {"choices": [{"index": 0, "delta": {"reasoning_content": "Hm."},
                                            "finish_reason": None}]}))
        plan[2] = (0.4, plan[2][1])
        self.fake.chat_plans = [plan]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "1", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertLess(json.loads(run.stdout)["results"][0]["ttft_ms"], 300)

    def test_bench_uses_the_engine_timings_when_the_stream_has_them(self):
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.05, timings={
            "prompt_n": 1000, "prompt_ms": 120, "prompt_per_second": 833.3,
            "predicted_n": 256, "predicted_ms": 480, "predicted_per_second": 533.3})]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "1", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        row = json.loads(run.stdout)["results"][0]
        self.assertEqual(row["source"], "engine")
        self.assert_close(row["prompt_tok_s"], 833.3)
        self.assert_close(row["decode_tok_s"], 533.3)

    def test_bench_stops_when_the_machine_is_off_and_never_wakes_it(self):
        self.fake.holds = []
        run = self.bench("chat-a")
        self.assertEqual(run.returncode, 3, run.stderr)
        self.assertIn("box is off", run.stderr)
        self.assertIn("--wake", run.stderr)
        self.assertEqual(self.requests("POST"), [])
        self.assertEqual(self.requests("POST", "/v1/chat/completions"), [])

    def test_bench_with_wake_holds_the_machine_and_lets_it_go(self):
        self.fake.holds = []
        self.fake.wake_makes_healthy = True
        self.fake.chat_plans = [chat_plan(delay=0.02)]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "1", "--wake")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/holds")["json"],
                         {"ttl_secs": 1800, "reason": "sb ai bench", "wake": True})
        self.assertEqual(len(self.requests("DELETE", "/v1/hosts/box/holds/hold-1")), 1)
        self.assertEqual(self.fake.holds, [])

    def test_bench_with_wake_releases_the_hold_when_a_run_fails(self):
        self.fake.holds = []
        self.fake.wake_makes_healthy = True
        self.fake.chat_plans = [chat_plan(delay=0.02)]
        self.fake.chat_fail_at = 2  # the second request of the run fails
        run = self.bench("chat-a", "--workload", "chat", "--runs", "3", "--wake")
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("the bench request failed", run.stderr)
        self.assertEqual(self.posted("/v1/hosts/box/holds")["json"],
                         {"ttl_secs": 1800, "reason": "sb ai bench", "wake": True})
        self.assertEqual(len(self.requests("DELETE", "/v1/hosts/box/holds/hold-1")), 1)
        self.assertEqual(self.fake.holds, [])

    def test_bench_with_wake_releases_the_hold_when_interrupted(self):
        self.fake.holds = []
        self.fake.wake_makes_healthy = True
        self.fake.chat_plans = [chat_plan(delay=5.0)]  # a slow answer to interrupt
        process = subprocess.Popen(["zsh", str(SB), "ai", "bench", "chat-a", "--workload", "chat",
                                    "--runs", "1", "--wake"],
                                   env=self.env, cwd=self.root, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, start_new_session=True)
        deadline = time.time() + 20
        while time.time() < deadline and not self.requests("POST", "/v1/chat/completions"):
            time.sleep(0.05)
        self.assertTrue(self.requests("POST", "/v1/chat/completions"), "the bench never sent a request")
        os.killpg(process.pid, signal.SIGINT)  # what Ctrl-C sends to the foreground group
        stdout, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 130, stderr)
        self.assertEqual(len(self.requests("DELETE", "/v1/hosts/box/holds/hold-1")), 1)
        self.assertEqual(self.fake.holds, [])

    def test_bench_json_shape(self):
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.02)]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "1", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        report = json.loads(run.stdout)
        self.assertEqual(sorted(report), ["models", "results", "runs", "workloads"])
        self.assertEqual(report["models"], ["chat-a"])
        self.assertEqual(report["workloads"], ["chat"])
        self.assertEqual(report["runs"], 1)
        self.assertEqual(len(report["results"]), 1)
        row = report["results"][0]
        self.assertEqual(sorted(row), ["completion_tokens", "decode_tok_s", "kind", "model",
                                       "prompt_tok_s", "prompt_tokens", "runs", "source",
                                       "total_s", "ttft_ms", "workload"])
        self.assertEqual(row["model"], "chat-a")
        self.assertEqual(row["workload"], "chat")
        self.assertEqual(row["kind"], "cold")
        self.assertEqual(row["runs"], 1)
        self.assertEqual(row["source"], "client")
        self.assertIsInstance(row["ttft_ms"], float)
        self.assertIsInstance(row["total_s"], float)
        self.assertIsInstance(row["prompt_tokens"], int)
        self.assertIsInstance(row["completion_tokens"], int)

    def test_bench_splits_cold_and_warm_and_takes_the_median(self):
        self.healthy()
        # the later runs answer in 300, 450 and 900 ms: the median is 450, not the mean
        self.fake.chat_plans = [chat_plan(delay=0.15), chat_plan(delay=0.3),
                                chat_plan(delay=0.45), chat_plan(delay=0.9)]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "4", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        rows = json.loads(run.stdout)["results"]
        self.assertEqual([(row["kind"], row["runs"]) for row in rows], [("cold", 1), ("warm", 3)])
        cold, warm = rows
        self.assert_close(cold["ttft_ms"], 150, delta=60)
        self.assert_close(warm["ttft_ms"], 450, delta=50)
        self.assertGreater(warm["ttft_ms"], cold["ttft_ms"])

    def test_bench_runs_both_workloads_and_several_models(self):
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.02)]
        run = self.bench("chat-a", "chat-b", "--workload", "all", "--runs", "1", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        report = json.loads(run.stdout)
        self.assertEqual(report["models"], ["chat-a", "chat-b"])
        self.assertEqual(report["workloads"], ["chat", "read"])
        self.assertEqual([(row["model"], row["workload"]) for row in report["results"]],
                         [("chat-a", "chat"), ("chat-a", "read"),
                          ("chat-b", "chat"), ("chat-b", "read")])
        bodies = [r["json"] for r in self.requests("POST", "/v1/chat/completions")]
        self.assertEqual(len(bodies), 4)
        self.assertEqual([b["max_tokens"] for b in bodies], [256, 16, 256, 16])
        for body in bodies:
            self.assertEqual(body["temperature"], 0)
            self.assertEqual(body["seed"], 1)
            self.assertEqual(body["reasoning_effort"], "none")
            self.assertTrue(body["stream"])
            self.assertEqual(body["stream_options"], {"include_usage": True})
        # the reading job is a long document, the chat a shorter note
        self.assertLess(len(bodies[0]["messages"][0]["content"]), len(bodies[1]["messages"][0]["content"]))

    def test_bench_prints_the_table(self):
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.02)]
        run = self.bench("chat-a", "--workload", "chat", "--runs", "1")
        self.assertEqual(run.returncode, 0, run.stderr)
        lines = run.stdout.splitlines()
        self.assertEqual(lines[0].split(), ["model", "workload", "kind", "runs", "ttft", "ms",
                                            "prompt", "tok/s", "decode", "tok/s", "total", "s", "source"])
        self.assertEqual(lines[1].split()[:4], ["chat-a", "chat", "cold", "1"])
        self.assertEqual(lines[1].split()[-1], "client")
        self.assertEqual(len(lines), 2)

    def test_bench_writes_the_json_to_a_file(self):
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.02)]
        out = self.root / "bench.json"
        run = self.bench("chat-a", "--workload", "chat", "--runs", "1", "-o", str(out))
        self.assertEqual(run.returncode, 0, run.stderr)
        report = json.loads(out.read_text())
        self.assertEqual(report["models"], ["chat-a"])
        self.assertEqual(report["results"][0]["kind"], "cold")
        self.assertIn("ttft ms", run.stdout)  # the table still goes to stdout

    def test_bench_needs_a_model_the_gateway_offers(self):
        self.healthy()
        run = self.bench("nope")
        self.assertEqual(run.returncode, 2, run.stderr)
        self.assertIn("unknown text model nope", run.stderr)
        self.assertEqual(self.requests("POST", "/v1/chat/completions"), [])

    def test_bench_takes_a_route_named_like_a_provider_id(self):
        # Route `mac/chat-a` on a lane `mac` whose provider is also `mac`: the plain
        # list hides it as an internal id, but named on its own it is benched.
        self.fake.lanes = [lane("lane-text", "healthy"),
                           lane("mac", "healthy", host="desk", wake_configured=False,
                                poweroff_configured=False)]
        self.fake.models = [{"id": "chat-a"}, {"id": "mac"}, {"id": "mac/chat-a"}]
        run = self.bench("mac/chat-a", "--runs", "1", "--workload", "chat")
        self.assertEqual(run.returncode, 0, run.stderr)
        sent = [r["json"]["model"] for r in self.requests("POST", "/v1/chat/completions")]
        self.assertEqual(sent, ["mac/chat-a"])
        self.assertEqual(self.requests("POST", "/v1/hosts/desk/holds"), [])

    def test_bench_without_a_model_uses_the_only_text_model(self):
        self.fake.models = [{"id": "chat-a"}]
        self.healthy()
        self.fake.chat_plans = [chat_plan(delay=0.02)]
        run = self.bench("--workload", "chat", "--runs", "1", "--json")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(json.loads(run.stdout)["models"], ["chat-a"])

    def test_bench_without_a_model_asks_which_one_when_there_are_several(self):
        self.healthy()
        run = self.bench("--workload", "chat", "--runs", "1")
        self.assertEqual(run.returncode, 2, run.stderr)
        self.assertIn("choose a text model to bench", run.stderr)
        self.assertIn("chat-a, chat-b", run.stderr)
        self.assertEqual(self.requests("POST", "/v1/chat/completions"), [])

    def test_bench_help(self):
        run = self.sb("help")
        self.assertIn("sb ai bench", run.stdout)
        self.assertIn("--workload", run.stdout)
        self.assertEqual(self.fake.seen, [])

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

    # ------------------------------------------------------------ model settings

    def test_set_writes_typed_values_into_a_profile_with_its_revision(self):
        run = self.sb("set", "text/m", "temperature=0.6", "reasoning.enabled=false",
                      'stop=["\\n\\n"]', "system_prompt=Be brief.", "--profile", "nothink", "--lock")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("text/m: revision 1", run.stdout)
        put = self.posted_with("PUT", "/v1/models/text/m/settings")
        self.assertEqual(put["headers"]["If-Match"], "0")
        self.assertEqual(put["json"]["profiles"]["nothink"],
                         {"temperature": 0.6, "reasoning": {"enabled": False},
                          "stop": ["\n\n"], "system_prompt": "Be brief."})
        self.assertEqual(sorted(put["json"]["locked"]),
                         ["reasoning.enabled", "stop", "system_prompt", "temperature"])

        run = self.sb("unset", "text/m", "temperature", "--profile", "nothink")
        self.assertEqual(run.returncode, 0, run.stderr)
        puts = self.requests("PUT", "/v1/models/text/m/settings")
        self.assertEqual(puts[-1]["headers"]["If-Match"], "1")
        self.assertNotIn("temperature", puts[-1]["json"]["profiles"]["nothink"])
        self.assertNotIn("temperature", puts[-1]["json"]["locked"])

    def test_set_on_a_name_shared_with_its_first_target_writes_that_target(self):
        self.fake.settings_fallback = True
        run = self.sb("set", "text/m", "top_k=20")
        self.assertEqual(run.returncode, 0, run.stderr)
        put = self.posted_with("PUT", "/v1/models/text/m/settings")
        self.assertEqual(put["json"]["defaults"], {"top_k": 20})
        self.assertEqual(self.requests("PUT", "/v1/models/other/m/settings"), [])

    def test_set_load_keys_need_load_and_carry_apply(self):
        run = self.sb("set", "text/m", "ctx_size=65536")
        self.assertEqual(run.returncode, 2)
        self.assertIn("--load", run.stderr)
        self.assertEqual(self.requests("PUT"), [])
        run = self.sb("set", "text/m", "ctx_size=65536", "cache_type_k=q8_0", "--load",
                      "--apply", "now", "--wake")
        self.assertEqual(run.returncode, 0, run.stderr)
        put = self.posted_with("PUT", "/v1/models/text/m/settings")
        self.assertEqual(put["json"]["load"], {"ctx_size": 65536, "cache_type_k": "q8_0"})
        self.assertEqual(put["json"]["apply"], "now")
        self.assertTrue(put["json"]["wake"])
        self.assertIn("load settings written", run.stdout)

    def test_set_loses_a_race_with_exit_1(self):
        self.fake.settings_conflict = True
        run = self.sb("set", "text/m", "top_k=20")
        self.assertEqual(run.returncode, 1)
        self.assertIn("changed meanwhile", run.stderr)

    def test_unknown_request_key_is_a_usage_error(self):
        run = self.sb("set", "text/m", "tempreature=1")
        self.assertEqual(run.returncode, 2)
        self.assertEqual(self.requests("PUT"), [])

    def test_settings_table_history_and_revert(self):
        self.fake.settings_doc = {"defaults": {"top_k": 20}, "locked": ["top_k"],
                                  "profiles": {"nothink": {"reasoning": {"enabled": False}}}}
        self.fake.settings_revision = 3
        self.fake.settings_history = [{"revision": 3, "at_ms": 0, "actor": "tenant=ops",
                                       "role": "admin", "action": "patch",
                                       "changed": ["defaults.top_k"]}]
        run = self.sb("settings", "text/m")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("text/m  revision 3 (durable)", run.stdout)
        self.assertRegex(run.stdout, r"top_k\s+20\s+defaults\s+locked")
        self.assertRegex(run.stdout, r"temperature\s+-\s+engine")
        self.assertIn("profile nothink:", run.stdout)
        run = self.sb("settings", "text/m", "--json")
        self.assertEqual(json.loads(run.stdout)["revision"], 3)
        run = self.sb("settings", "text/m", "--history")
        self.assertIn("patch: defaults.top_k", run.stdout)
        run = self.sb("revert", "text/m", "3")
        self.assertEqual(run.returncode, 0, run.stderr)
        revert = self.posted("/v1/models/text/m/settings/revert")
        self.assertEqual(revert["json"], {"revision": 3})
        self.assertEqual(revert["headers"]["If-Match"], "3")
        self.assertIn("(the settings of revision 3)", run.stdout)
        run = self.sb("revert", "text/m", "2")
        self.assertEqual(run.returncode, 1)
        self.assertIn("no settings revision 2", run.stderr)

    def test_revert_to_zero_means_no_saved_settings(self):
        self.fake.settings_doc = {"defaults": {"top_k": 20}, "locked": ["top_k"],
                                  "load": {"ctx_size": 8192}}
        self.fake.settings_revision = 3
        run = self.sb("revert", "text/m", "0")
        self.assertEqual(run.returncode, 0, run.stderr)
        revert = self.posted("/v1/models/text/m/settings/revert")
        self.assertEqual(revert["json"], {"revision": 0})
        self.assertEqual(revert["headers"]["If-Match"], "3")
        self.assertIn("text/m: revision 4 (no saved settings", run.stdout)
        self.assertEqual(self.fake.settings_doc, {})
        self.assertEqual(self.requests("PUT"), [])
        run = self.sb("revert", "text/m", "-1")
        self.assertEqual(run.returncode, 2)

    def test_revert_to_zero_on_an_older_gateway_writes_an_empty_document(self):
        # A gateway that cannot revert to 0 gets the same write as `unset`
        # of every key: an empty document, with the revision it edits.
        self.fake.revert_zero_unknown = True
        self.fake.settings_doc = {"defaults": {"top_k": 20}, "locked": ["top_k"],
                                  "load": {"ctx_size": 8192}}
        self.fake.settings_revision = 3
        run = self.sb("revert", "text/m", "0")
        self.assertEqual(run.returncode, 0, run.stderr)
        put = self.posted_with("PUT", "/v1/models/text/m/settings")
        self.assertEqual(put["json"], {})
        self.assertEqual(put["headers"]["If-Match"], "3")
        self.assertIn("text/m: revision 4 (no saved settings", run.stdout)

    def test_chat_sends_profile_and_settings_and_prints_stats(self):
        self.fake.chat_headers = {"x-switchback-route": "strategy=pinned_order selected=text/m",
                                  "x-switchback-settings": "profile=nothink; applied=2"}
        run = self.sb("chat", "fast", "hi there", "--profile", "nothink", "--set", "top_k=5",
                      "--set", "reasoning.enabled=false")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout, "The gateway keeps the record.\n")
        body = self.posted("/v1/chat/completions")["json"]
        self.assertEqual(body["switchback"], {"profile": "nothink",
                                              "settings": {"top_k": 5, "reasoning": {"enabled": False}}})
        self.assertTrue(body["stream"])
        self.assertIn("tokens: 1000 in, 256 out", run.stderr)
        self.assertIn("route: strategy=pinned_order selected=text/m", run.stderr)
        self.assertIn("settings: profile=nothink; applied=2", run.stderr)
        run = self.sb("chat", "fast", "hi", "--load", "ctx_size=65536")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.requests("POST", "/v1/chat/completions")[-1]["json"]["switchback"],
                         {"load": {"ctx_size": 65536}})
        run = self.sb("chat", "fast", "hi", "--json")
        result = json.loads(run.stdout)
        self.assertEqual(result["completion_tokens"], 256)
        self.assertNotIn("switchback", self.requests("POST", "/v1/chat/completions")[-1]["json"])

    def test_load_unload_and_engines(self):
        self.fake.machine_asleep = True
        run = self.sb("load", "text/m")
        self.assertEqual(run.returncode, 3)
        self.assertIn("asleep", run.stderr)
        run = self.sb("load", "text/m", "--wake")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("text/m: loaded (3.0 GiB on GPU at ctx 4096)", run.stdout)
        self.assertEqual(self.requests("POST", "/v1/models/text/m/load")[-1]["json"], {"wake": True})
        run = self.sb("unload", "text/m")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("text/m: unloaded", run.stdout)
        run = self.sb("engines")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("text (llama_cpp_router): asleep", run.stdout)
        self.assertIn("m  loaded  (2.0 GiB at ctx 4096; settings pending until reload)", run.stdout)
        self.assertIn("last apply failed: exit 2: pinned", run.stdout)

    def test_hold_can_name_a_model_to_load(self):
        self.fake.holds = []
        run = self.sb("hold", "2h", "eval", "--wake", "--load", "text/m", "--host", "box")
        self.assertEqual(run.returncode, 0, run.stderr)
        posted = [r for r in self.requests("POST") if r["path"].endswith("/holds")]
        self.assertEqual(posted[-1]["json"]["load"], "text/m")

    def posted_with(self, method, path):
        found = self.requests(method, path)
        self.assertEqual(len(found), 1, f"expected one {method} {path}")
        return found[0]


if __name__ == "__main__":
    unittest.main()
