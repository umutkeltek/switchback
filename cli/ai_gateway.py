#!/usr/bin/env python3
"""sb ai: images, video and machine status through a Switchback gateway.

The gateway is the one door: it wakes a managed machine when work arrives and
powers it off again when the machine has been idle. This client never wakes a
machine by itself; only `sb ai hold --wake` asks the gateway to.

Standard library only. The gateway key is read from the environment or from
the config file, sent only in the Authorization header, and never printed.
"""

import argparse
import json
import os
import random
import re
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

OK, GATEWAY_ERROR, USAGE, UNREACHABLE = 0, 1, 2, 3

HELP = """\
sb ai: images, video and machine status through a Switchback gateway

  sb ai image "prompt" [--model NAME] [--aspect W:H] [--mp N] [--size WxH] [--seed N] [-o FILE]
  sb ai video "prompt" --image FIRST_FRAME [--model NAME] [--seconds N] [--frames N]
                       [--aspect W:H] [--mp N] [--size WxH] [--seed N] [-o FILE]
  sb ai status [--json]          lanes and machine power; never wakes anything
  sb ai models [--json]          what the gateway offers, by job (text, image, video)
  sb ai hold DURATION [REASON] [--wake] [--host NAME]
                                 keep the machine on for direct work (30m, 2h, 1h30m)
  sb ai release [ID | --all] [--host NAME]
                                 let the machine go again

image and video print the saved file path on stdout; progress goes to stderr.
They wait up to --timeout SECS (default 900). hold takes --json for the raw answer.
--model takes the full id or its last part (flux for <lane>/flux or image/flux).
Ctrl-C during a video cancels the job on the gateway.

Config: SWITCHBACK_AI_URL and SWITCHBACK_AI_KEY in the environment, else in
${XDG_CONFIG_HOME:-~/.config}/switchback/ai.env (KEY=VALUE lines). Optional there too:
SWITCHBACK_AI_IMAGE_MODEL, SWITCHBACK_AI_VIDEO_MODEL (defaults), SWITCHBACK_AI_OUT_DIR.
Exit codes: 0 ok, 1 the gateway reported an error, 2 usage error, 3 gateway unreachable.
"""

CONFIG_KEYS = ("SWITCHBACK_AI_URL", "SWITCHBACK_AI_KEY", "SWITCHBACK_AI_IMAGE_MODEL",
               "SWITCHBACK_AI_VIDEO_MODEL", "SWITCHBACK_AI_OUT_DIR")
IMAGE_KIND, VIDEO_KIND = "image_generation", "video_generation"
VIDEO_FPS = 24
POLL_SECS = float(os.environ.get("SB_AI_POLL_SECS") or 2.0)  # tests shorten it
VIDEO_MP = 0.4
LANE_WORDS = {"offline": "off", "waking": "waking", "healthy": "idle", "draining": "busy",
              "idle_countdown": "idle, power-off countdown", "powering_off": "powering off",
              "degraded": "needs attention"}
EXTENSIONS = {"image/png": ".png", "image/jpeg": ".jpg", "image/webp": ".webp", "image/gif": ".gif",
              "video/mp4": ".mp4", "video/webm": ".webm", "video/quicktime": ".mov"}


class Fail(Exception):
    """A stop with an exit code and one plain message."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


class GatewayError(Exception):
    """The gateway answered with an HTTP error."""

    def __init__(self, status, message, has_body):
        super().__init__(message)
        self.status = status
        self.message = message
        self.has_body = has_body  # False for a bare 404: the route does not exist


# ---------------------------------------------------------------- config

def config_path(environ):
    base = environ.get("XDG_CONFIG_HOME") or os.path.join(os.path.expanduser("~"), ".config")
    return os.path.join(base, "switchback", "ai.env")


def read_env_file(path):
    values = {}
    try:
        with open(path, encoding="utf-8") as handle:
            lines = handle.read().splitlines()
    except FileNotFoundError:
        return values
    except OSError as error:
        raise Fail(USAGE, f"cannot read {path}: {error.strerror}")
    for line in lines:
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, value = line.split("=", 1)
        name = name.strip()
        if name.startswith("export "):
            name = name[len("export "):].strip()
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
            value = value[1:-1]
        values[name] = value
    return values


def load_config(environ):
    path = config_path(environ)
    values = read_env_file(path)
    config = {name: values.get(name, "") for name in CONFIG_KEYS}
    for name in CONFIG_KEYS:
        if environ.get(name):
            config[name] = environ[name]  # the environment wins
    if not config["SWITCHBACK_AI_URL"]:
        raise Fail(USAGE, f"no gateway address: set SWITCHBACK_AI_URL, or write it in {path}")
    parsed = urllib.parse.urlsplit(config["SWITCHBACK_AI_URL"])
    if parsed.scheme not in ("http", "https") or not parsed.netloc:
        raise Fail(USAGE, "SWITCHBACK_AI_URL must look like http://HOST:PORT")
    return config


# ---------------------------------------------------------------- gateway

class Gateway:
    def __init__(self, url, key):
        self.base = url.rstrip("/")
        self.key = key
        # The gateway is on the local network: never route it through a proxy
        # from the environment.
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def scrub(self, text):
        text = str(text)
        return text.replace(self.key, "[key]") if self.key else text

    def call(self, method, path, body=None, content_type=None, timeout=30, raw=False):
        headers = {"Accept": "application/json"}
        if self.key:
            headers["Authorization"] = f"Bearer {self.key}"
        if body is not None and not isinstance(body, bytes):
            body = json.dumps(body).encode()
            content_type = "application/json"
        if content_type:
            headers["Content-Type"] = content_type
        request = urllib.request.Request(self.base + path, data=body, headers=headers, method=method)
        try:
            with self.opener.open(request, timeout=timeout) as response:
                data = response.read()
        except urllib.error.HTTPError as error:
            raise self._http_error(error) from None
        except urllib.error.URLError as error:
            raise Fail(UNREACHABLE, f"the gateway does not answer ({self.scrub(reason_of(error))})") from None
        except (TimeoutError, socket.timeout):
            raise Fail(GATEWAY_ERROR, f"no answer from the gateway after {timeout:.0f}s") from None
        except OSError as error:
            raise Fail(UNREACHABLE, f"the gateway connection broke ({self.scrub(reason_of(error))})") from None
        if raw:
            return data
        try:
            return json.loads(data or b"null")
        except ValueError:
            raise Fail(GATEWAY_ERROR, f"the gateway sent an answer that is not JSON on {path}") from None

    def _http_error(self, error):
        try:
            payload = error.read()
        except OSError:
            payload = b""
        message, has_body = "", False
        try:
            parsed = json.loads(payload)
            if isinstance(parsed, dict):
                inner = parsed.get("error")
                message = inner.get("message", "") if isinstance(inner, dict) else str(inner or "")
                has_body = True
        except ValueError:
            pass
        return GatewayError(error.code, self.scrub(message or error.reason or "error"), has_body)

    def get(self, path, **kw):
        return self.call("GET", path, **kw)

    def post(self, path, body=None, **kw):
        return self.call("POST", path, body=body if body is not None else {}, **kw)

    def delete(self, path, **kw):
        return self.call("DELETE", path, **kw)

    def answers(self):
        try:
            self.call("GET", "/health", timeout=5, raw=True)
            return True
        except (Fail, GatewayError):
            return False


def reason_of(error):
    return getattr(error, "reason", None) or error


def log(message):
    print(message, file=sys.stderr, flush=True)


def gateway_fail(error, doing):
    return Fail(GATEWAY_ERROR, f"{doing}: the gateway said {error.status}: {error.message}")


# ---------------------------------------------------------------- models

def catalog(gw):
    """{"text": [...], "image": [...], "video": [...], "listed": bool, "test": set}."""
    result = {"text": [], "image": [], "video": [], "listed": False, "test": set()}
    try:
        models = gw.get("/v1/models")
    except GatewayError as error:
        raise gateway_fail(error, "listing models")
    try:
        workflows = gw.get("/v1/workflows")
        result["listed"] = True
    except GatewayError as error:
        if error.status not in (401, 403, 404):
            raise gateway_fail(error, "listing workflows")
        workflows = {"data": []}  # this key may not list workflows; media ids are then not known
    for row in workflows.get("data") or []:
        kind = row.get("kind")
        group = "image" if kind == IMAGE_KIND else "video" if kind == VIDEO_KIND else None
        if group and row.get("id") and row["id"] not in result[group]:
            result[group].append(row["id"])
            if str(row.get("provider", "")).endswith("-mock"):
                result["test"].add(row["id"])
    media = set(result["image"]) | set(result["video"])
    for row in models.get("data") or []:
        model_id = row.get("id") if isinstance(row, dict) else None
        if not model_id or model_id in media:
            continue
        prefix = model_id.split("/", 1)[0]
        if prefix in ("image", "video") and "/" in model_id:
            result[prefix].append(model_id)
            media.add(model_id)
        else:
            result["text"].append(model_id)
    return result


def resolve_model(name, group, cat, default=""):
    other = "video" if group == "image" else "image"
    choices = cat[group]
    real = [c for c in choices if c not in cat["test"]]
    name = name or default
    if not name:
        if len(real) == 1:
            return real[0]
        listed = ", ".join(real) or "none"
        raise Fail(USAGE, f"choose a {group} model with --model (the gateway offers: {listed})")
    if name in choices:
        return name
    if not cat["listed"] and "/" in name:
        return name  # this key cannot list workflows; the gateway checks the id
    matches = [c for c in choices if c.rsplit("/", 1)[-1] == name]
    if len(matches) == 1:
        return matches[0]
    if len(matches) > 1:
        raise Fail(USAGE, f"--model {name} matches several {group} models: {', '.join(matches)}; "
                          "give the full id")
    if name in cat[other] or any(c.rsplit("/", 1)[-1] == name for c in cat[other]):
        raise Fail(USAGE, f"--model {name} is a {other} model, not a {group} model")
    if not cat["listed"]:
        raise Fail(USAGE, f"this key cannot list the gateway's {group} models; give the full id "
                          f"(like <lane>/{name})")
    listed = ", ".join(choices) or "none"
    raise Fail(USAGE, f"unknown {group} model {name}; the gateway offers: {listed}")


# ---------------------------------------------------------------- capacity

def capacity(gw, quiet=False):
    """The capacity report (GET only: reading it never wakes a machine)."""
    try:
        return gw.get("/v1/workloads/capacity", timeout=15)
    except GatewayError as error:
        if quiet:
            return None
        if error.status in (401, 403):
            raise Fail(GATEWAY_ERROR, "this key may not read the gateway's capacity "
                                      f"(the gateway said {error.status}: {error.message})")
        raise gateway_fail(error, "reading capacity")
    except Fail:
        if quiet:
            return None
        raise


def machine_of(lane):
    return lane.get("host") or lane.get("name") or "?"


def power_of(states):
    on = {"healthy", "draining", "idle_countdown", "degraded"}
    if states & on:
        return "on"
    if "waking" in states:
        return "waking"
    if "powering_off" in states:
        return "powering off"
    return "off"


def machines(lanes):
    order, states = [], {}
    for lane in lanes:
        key = machine_of(lane)
        if key not in states:
            order.append(key)
            states[key] = set()
        states[key].add(lane.get("state"))
    return [{"machine": key, "power": power_of(states[key])} for key in order]


def duration_words(secs):
    secs = max(0, int(secs))
    if secs < 90:
        return f"{secs}s"
    if secs < 5400:
        return f"{round(secs / 60)} min"
    return f"{secs / 3600:.1f} h"


def holds_words(holds):
    if isinstance(holds, int):
        return f"{holds} hold{'s' if holds != 1 else ''}"
    if not isinstance(holds, list) or not holds:
        return ""
    parts = []
    for hold in holds:
        if not isinstance(hold, dict):
            parts.append(str(hold))
            continue
        bits = [str(hold.get("id", "?"))]
        if hold.get("reason"):
            bits.append(str(hold["reason"]))
        left = hold_left(hold)
        if left is not None:
            bits.append(f"{duration_words(left)} left")
        parts.append(" ".join(bits))
    return f"{len(holds)} hold{'s' if len(holds) != 1 else ''} ({'; '.join(parts)})"


def hold_left(hold):
    for key in ("remaining_secs", "expires_in_secs", "ttl_left_secs"):
        if isinstance(hold.get(key), (int, float)):
            return hold[key]
    if isinstance(hold.get("expires_at_ms"), (int, float)):
        return hold["expires_at_ms"] / 1000 - time.time()
    return None


def lane_line(lane):
    state = lane.get("state", "?")
    parts = [f"{LANE_WORDS.get(state, state)}", f"level {lane.get('level', '?')}",
             f"queue {lane.get('queue_depth', 0)}"]
    if lane.get("in_flight"):
        parts.append(f"running {lane['in_flight']}")
    if lane.get("holds_gpu") is not None:
        parts.append("has the GPU" if lane["holds_gpu"] else "GPU with another lane")
    if isinstance(lane.get("poweroff_in_secs"), (int, float)):
        parts.append(f"powers off in {duration_words(lane['poweroff_in_secs'])}")
    if lane.get("poweroff_guard_busy"):
        parts.append("power-off postponed: the machine reports another user")
    words = holds_words(lane.get("holds"))
    if words:
        parts.append(words)
    if state == "degraded" and lane.get("last_error"):
        parts.append(f"error: {lane['last_error']}")
    return f"  {lane.get('name', '?')}: " + ", ".join(parts)


def cmd_status(gw, args):
    report = capacity(gw)
    lanes = report.get("data") or []
    if args.json:
        print(json.dumps({"machines": machines(lanes), "lanes": lanes}, indent=2))
        return OK
    if not lanes:
        print("the gateway manages no machine (no local lanes)")
        return OK
    for machine in machines(lanes):
        print(f"{machine['machine']}: {machine['power']}")
        for lane in lanes:
            if machine_of(lane) == machine["machine"]:
                print(lane_line(lane))
    return OK


def cmd_models(gw, args):
    cat = catalog(gw)
    groups = {name: cat[name] for name in ("text", "image", "video")}
    if args.json:
        print(json.dumps(groups, indent=2))
        return OK
    for name, ids in groups.items():
        print(f"{name}:")
        for model_id in ids:
            note = " (test)" if model_id in cat["test"] else ""
            print(f"  {model_id}{note}")
        if not ids:
            print("  (none)")
    if not cat["listed"]:
        print("(this key cannot list workflows; image and video may be missing)")
    return OK


# ---------------------------------------------------------------- outputs

def output_path(args, config, model, tag, index, media_type):
    ext = EXTENSIONS.get(media_type, ".bin")
    if args.output:
        if index == 0:
            return os.path.abspath(args.output)
        stem, given = os.path.splitext(args.output)
        return os.path.abspath(f"{stem}-{index}{given or ext}")
    folder = config["SWITCHBACK_AI_OUT_DIR"] or os.getcwd()
    os.makedirs(folder, exist_ok=True)
    short = re.sub(r"[^A-Za-z0-9._-]+", "-", model.rsplit("/", 1)[-1])
    suffix = f"-{index}" if index else ""
    return os.path.abspath(os.path.join(folder, f"ai-{time.strftime('%Y%m%d-%H%M%S')}-{short}-{tag}{suffix}{ext}"))


def save_artifact(gw, artifact, path, timeout):
    try:
        data = gw.get(f"/v1/artifacts/{urllib.parse.quote(artifact['artifact_id'], safe='')}",
                      timeout=timeout, raw=True)
    except GatewayError as error:
        raise gateway_fail(error, f"downloading {artifact['artifact_id']}")
    part = path + ".part"
    with open(part, "wb") as handle:
        handle.write(data)
    os.replace(part, path)
    print(path, flush=True)


def parse_size(text):
    match = re.fullmatch(r"(\d+)x(\d+)", text or "")
    if not match or int(match[1]) == 0 or int(match[2]) == 0:
        raise Fail(USAGE, f"--size wants WIDTHxHEIGHT, like 1024x768 (got {text})")
    return text


def parse_aspect(text):
    match = re.fullmatch(r"(\d+(?:\.\d+)?):(\d+(?:\.\d+)?)", text or "")
    if not match or float(match[1]) == 0 or float(match[2]) == 0:
        raise Fail(USAGE, f"--aspect wants W:H, like 16:9 (got {text})")
    return float(match[1]) / float(match[2])


def size_for(ratio, megapixels):
    """WIDTHxHEIGHT of about `megapixels` at `ratio`, both multiples of 32."""
    pixels = megapixels * 2 ** 20
    width = max(32, int(round((pixels * ratio) ** 0.5 / 32)) * 32)
    height = max(32, int(round((pixels / ratio) ** 0.5 / 32)) * 32)
    return f"{width}x{height}"


def note_machine(gw):
    report = capacity(gw, quiet=True)
    if not report:
        return
    for machine in machines(report.get("data") or []):
        if machine["power"] in ("off", "powering off"):
            log(f"{machine['machine']} is {machine['power']}; the gateway wakes it for this job "
                "(from off this takes a minute or two)")


# ---------------------------------------------------------------- image

def cmd_image(gw, config, args):
    if args.size and (args.aspect or args.mp):
        raise Fail(USAGE, "give --size, or --aspect/--mp, not both")
    if args.aspect:
        parse_aspect(args.aspect)
    if args.mp is not None and args.mp <= 0:
        raise Fail(USAGE, "--mp must be above 0")
    model = resolve_model(args.model, "image", catalog(gw), config["SWITCHBACK_AI_IMAGE_MODEL"])
    body = {"model": model, "prompt": args.prompt, "n": 1}
    if args.seed is not None:
        body["seed"] = args.seed
    if args.size:
        body["size"] = parse_size(args.size)
    metadata = {}
    if args.aspect:
        metadata["aspect_ratio"] = args.aspect
    if args.mp is not None:
        metadata["megapixels"] = f"{args.mp:g}"
    if metadata:
        body["metadata"] = metadata
    note_machine(gw)
    log(f"generating with {model}" + (f" (seed {args.seed})" if args.seed is not None else ""))
    started = time.time()
    try:
        answer = gw.post("/v1/images/generations", body, timeout=args.timeout)
    except GatewayError as error:
        raise gateway_fail(error, "the image failed")
    job = answer.get("job") or {}
    images = answer.get("data") or []
    if not images:
        raise Fail(GATEWAY_ERROR, f"job {job.get('id', '?')} ended {job.get('status', '?')} without an image")
    log(f"job {job.get('id', '?')} done in {time.time() - started:.0f}s")
    tag = str(args.seed) if args.seed is not None else str(job.get("id", "job"))[-8:]
    for index, image in enumerate(images):
        save_artifact(gw, image, output_path(args, config, model, tag, index, image.get("media_type")), args.timeout)
    return OK


# ---------------------------------------------------------------- video

def image_type(head):
    if head[:8] == b"\x89PNG\r\n\x1a\n":
        return "image/png"
    if head[:3] == b"\xff\xd8\xff":
        return "image/jpeg"
    if head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return "image/webp"
    return None


def wait_words(job, gw):
    """Why a queued job waits, in plain words."""
    events = job.get("events") or []
    waiting = next((e for e in reversed(events) if e.get("event") == "waiting_for_lane"), None)
    if not waiting:
        return "queued"
    detail = waiting.get("detail") or {}
    lane_name, state = detail.get("lane"), detail.get("lane_state")
    report = capacity(gw, quiet=True) if lane_name else None
    lane = next((l for l in (report or {}).get("data") or [] if l.get("name") == lane_name), {})
    state = lane.get("state") or state
    if state == "degraded":
        return f"the lane needs attention: {lane.get('last_error') or lane.get('detail') or 'no detail'}"
    if state in ("offline", "waking"):
        return "waiting for the machine to wake (from off this takes a minute or two)"
    if state == "powering_off":
        return "waiting: the machine is powering off; the gateway wakes it again"
    holds_gpu = lane.get("holds_gpu") if "holds_gpu" in lane else (
        detail["holds_gpu"] == "true" if "holds_gpu" in detail else None)
    if holds_gpu is False:
        return "waiting for the GPU (another lane has it; the gateway switches it over)"
    return "waiting for the lane"


STATUS_WORDS = {"accepted": "accepted", "routing": "starting", "leased": "starting",
                "running": "running", "artifact_ready": "saving the result"}


def job_failure(job):
    event = next((e for e in reversed(job.get("events") or []) if e.get("status") == "failed"), {})
    detail = event.get("detail") or {}
    return detail.get("error") or detail.get("error_class") or "no reason given"


def cmd_video(gw, config, args):
    if args.size and (args.aspect or args.mp):
        raise Fail(USAGE, "give --size, or --aspect/--mp, not both")
    if args.seconds is not None and args.frames is not None:
        raise Fail(USAGE, "give --seconds or --frames, not both")
    if args.seconds is not None and args.seconds <= 0:
        raise Fail(USAGE, "--seconds must be above 0")
    if args.frames is not None and args.frames <= 0:
        raise Fail(USAGE, "--frames must be above 0")
    if args.mp is not None and args.mp <= 0:
        raise Fail(USAGE, "--mp must be above 0")
    size = parse_size(args.size) if args.size else None
    if args.aspect or args.mp is not None:
        size = size_for(parse_aspect(args.aspect or "1:1"), args.mp or VIDEO_MP)
    try:
        with open(args.image, "rb") as handle:
            frame = handle.read()
    except OSError as error:
        raise Fail(USAGE, f"cannot read --image {args.image}: {error.strerror}")
    media_type = image_type(frame)
    if not media_type:
        raise Fail(USAGE, "--image must be a PNG, JPEG or WebP file")
    model = resolve_model(args.model, "video", catalog(gw), config["SWITCHBACK_AI_VIDEO_MODEL"])
    inputs = {"prompt": args.prompt}
    if args.seed is not None:
        inputs["seed"] = args.seed
    if args.frames is not None:
        inputs["frames"] = args.frames
    elif args.seconds is not None:
        inputs["frames"] = max(1, round(args.seconds * VIDEO_FPS))
    if size:
        inputs["size"] = size
    note_machine(gw)
    started, job_id = time.time(), None
    try:
        try:
            upload = gw.post("/v1/uploads", frame, content_type=media_type, timeout=120)
        except GatewayError as error:
            raise gateway_fail(error, "the first frame was refused")
        except Fail as failure:
            if failure.code == UNREACHABLE and gw.answers():
                raise Fail(GATEWAY_ERROR, f"the upload broke off ({len(frame) / 2 ** 20:.1f} MiB); the gateway "
                                          "may refuse files above its upload limit") from None
            raise
        inputs["image"] = upload["artifact_id"]
        log(f"first frame uploaded ({len(frame) // 1024} KB)")
        try:
            job = gw.post("/v1/jobs", {"kind": VIDEO_KIND, "model": model, "inputs": inputs})
        except GatewayError as error:
            raise gateway_fail(error, "the video was refused")
        job_id = job["id"]
        log(f"video job {job_id} started with {model}"
            + (f", {inputs['frames']} frames" if "frames" in inputs else "")
            + (f", {size}" if size else ""))
        job = poll(gw, job, args.timeout, started)
    except KeyboardInterrupt:
        if job_id:
            try:
                gw.post(f"/v1/jobs/{urllib.parse.quote(job_id, safe='')}/cancel", {}, timeout=20)
                log(f"\ncancelled job {job_id} on the gateway")
            except (Fail, GatewayError) as error:
                log(f"\ninterrupted; cancelling job {job_id} failed ({getattr(error, 'message', error)})")
        else:
            log("\ninterrupted before the job started")
        return 130
    videos = [a for a in job.get("artifacts") or []
              if a.get("kind") == "video" or str(a.get("media_type", "")).startswith("video/")]
    if not videos:
        raise Fail(GATEWAY_ERROR, f"job {job_id} succeeded without a video")
    log(f"job {job_id} done in {time.time() - started:.0f}s")
    tag = str(args.seed) if args.seed is not None else str(job_id)[-8:]
    for index, video in enumerate(videos):
        save_artifact(gw, video, output_path(args, config, model, tag, index, video.get("media_type")), args.timeout)
    return OK


def poll(gw, job, timeout, started):
    job_id = job["id"]
    path = f"/v1/jobs/{urllib.parse.quote(job_id, safe='')}"
    said, said_at, misses = None, 0.0, 0
    while True:
        status = job.get("status")
        if status == "succeeded":
            return job
        if status == "failed":
            raise Fail(GATEWAY_ERROR, f"job {job_id} failed: {job_failure(job)}")
        if status == "cancelled":
            raise Fail(GATEWAY_ERROR, f"job {job_id} was cancelled")
        words = wait_words(job, gw) if status == "queued" else STATUS_WORDS.get(status, status)
        now = time.time()
        if words != said or now - said_at >= 30:
            log(f"{now - started:5.0f}s  {words}")
            said, said_at = words, now
        if now - started > timeout:
            raise Fail(GATEWAY_ERROR, f"job {job_id} still runs after {timeout:.0f}s; it keeps going on the "
                                      f"gateway (cancel: POST /v1/jobs/{job_id}/cancel)")
        time.sleep(POLL_SECS)
        try:
            job = gw.get(path, timeout=15)
            misses = 0
        except GatewayError as error:
            raise gateway_fail(error, f"following job {job_id}")
        except Fail as failure:
            misses += 1
            if misses > 30:
                raise Fail(failure.code, f"lost the gateway while job {job_id} runs: {failure.message}")



# ---------------------------------------------------------------- holds

def parse_duration(text):
    match = re.fullmatch(r"(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?", text or "")
    if not text or not match or not any(match.groups()):
        raise Fail(USAGE, f"DURATION wants a unit, like 30m, 2h, 1h30m or 90s (got {text})")
    hours, minutes, seconds = (int(part or 0) for part in match.groups())
    total = hours * 3600 + minutes * 60 + seconds
    if total <= 0:
        raise Fail(USAGE, "DURATION must be above 0")
    return total


def host_for(gw, args):
    if args.host:
        return args.host
    lanes = (capacity(gw).get("data") or [])
    hosts = sorted({lane["host"] for lane in lanes if lane.get("host")})
    if len(hosts) == 1:
        return hosts[0]
    if not hosts:
        raise Fail(GATEWAY_ERROR, "the capacity report names no machine (no lane has a host)")
    raise Fail(USAGE, f"the gateway manages several machines ({', '.join(hosts)}); choose one with --host")


def holds_path(host, hold_id=None):
    path = f"/v1/hosts/{urllib.parse.quote(host, safe='')}/holds"
    return path + (f"/{urllib.parse.quote(hold_id, safe='')}" if hold_id else "")


def holds_error(error, doing):
    if error.status == 404 and not error.has_body:
        return Fail(GATEWAY_ERROR, "this gateway has no holds yet")
    return gateway_fail(error, doing)


def hold_list(answer):
    if isinstance(answer, list):
        return answer
    if isinstance(answer, dict):
        for key in ("data", "holds"):
            if isinstance(answer.get(key), list):
                return answer[key]
    return []


def hold_record(answer):
    if isinstance(answer, dict) and isinstance(answer.get("hold"), dict):
        return answer["hold"]
    return answer if isinstance(answer, dict) else {}


def cmd_hold(gw, args):
    ttl = parse_duration(args.duration)
    host = host_for(gw, args)
    body = {"ttl_secs": ttl, "reason": args.reason or "sb ai hold", "wake": bool(args.wake)}
    try:
        answer = gw.post(holds_path(host), body)
    except GatewayError as error:
        raise holds_error(error, "the hold was refused")
    if args.json:
        print(json.dumps(answer, indent=2))
        return OK
    hold = hold_record(answer)
    woke = "; the gateway wakes it" if args.wake else ""
    print(f"holding {host} on for {duration_words(ttl)}: hold {hold.get('id', '?')}{woke}")
    print(f"let it go: sb ai release {hold.get('id', '')}".rstrip())
    return OK


def cmd_release(gw, args):
    if args.all and args.id:
        raise Fail(USAGE, "give a hold ID or --all, not both")
    host = host_for(gw, args)
    ids = [args.id] if args.id else None
    if ids is None:
        try:
            holds = hold_list(gw.get(holds_path(host)))
        except GatewayError as error:
            raise holds_error(error, "listing holds")
        ids = [str(h.get("id")) for h in holds if isinstance(h, dict) and h.get("id")]
        if not ids:
            print(f"{host} has no holds")
            return OK
        if len(ids) > 1 and not args.all:
            raise Fail(USAGE, f"{host} has {len(ids)} holds ({', '.join(ids)}); give an ID or --all")
    for hold_id in ids:
        try:
            gw.delete(holds_path(host, hold_id))
        except GatewayError as error:
            raise holds_error(error, f"releasing hold {hold_id}")
        print(f"released hold {hold_id} on {host}")
    return OK


# ---------------------------------------------------------------- main

class Parser(argparse.ArgumentParser):
    def error(self, message):
        print(f"sb ai: {message}\n(see sb ai help)", file=sys.stderr)
        sys.exit(USAGE)


def parser():
    top = Parser(prog="sb ai", add_help=False, allow_abbrev=False)
    sub = top.add_subparsers(dest="command")
    for name in ("image", "video"):
        cmd = sub.add_parser(name, add_help=False, allow_abbrev=False)
        cmd.add_argument("prompt")
        cmd.add_argument("--model", default="")
        cmd.add_argument("--aspect")
        cmd.add_argument("--mp", type=float)
        cmd.add_argument("--size")
        cmd.add_argument("--seed", type=int)
        cmd.add_argument("-o", "--output")
        cmd.add_argument("--timeout", type=float, default=900)
        if name == "video":
            cmd.add_argument("--image", required=True)
            cmd.add_argument("--seconds", type=float)
            cmd.add_argument("--frames", type=int)
    for name in ("status", "models"):
        sub.add_parser(name, add_help=False, allow_abbrev=False).add_argument("--json", action="store_true")
    hold = sub.add_parser("hold", add_help=False, allow_abbrev=False)
    hold.add_argument("duration")
    hold.add_argument("reason", nargs="?", default="")
    hold.add_argument("--wake", action="store_true")
    hold.add_argument("--host")
    hold.add_argument("--json", action="store_true")
    release = sub.add_parser("release", add_help=False, allow_abbrev=False)
    release.add_argument("id", nargs="?")
    release.add_argument("--all", action="store_true")
    release.add_argument("--host")
    return top


def main(argv=None, environ=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    environ = os.environ if environ is None else environ
    if not argv or argv[0] == "help" or "-h" in argv or "--help" in argv:
        print(HELP, end="")
        return OK
    args = parser().parse_args(argv)
    gw = None
    try:
        config = load_config(environ)
        gw = Gateway(config["SWITCHBACK_AI_URL"], config["SWITCHBACK_AI_KEY"])
        if args.command == "image":
            return cmd_image(gw, config, args)
        if args.command == "video":
            return cmd_video(gw, config, args)
        if args.command == "status":
            return cmd_status(gw, args)
        if args.command == "models":
            return cmd_models(gw, args)
        if args.command == "hold":
            return cmd_hold(gw, args)
        if args.command == "release":
            return cmd_release(gw, args)
        raise Fail(USAGE, f"unknown command {args.command}")
    except Fail as failure:
        message = gw.scrub(failure.message) if gw else failure.message
        print(f"sb ai: {message}", file=sys.stderr)
        return failure.code
    except KeyboardInterrupt:
        print("\nsb ai: interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
