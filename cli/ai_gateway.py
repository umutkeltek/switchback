#!/usr/bin/env python3
"""sb ai: images, video, audio and machine status through a Switchback gateway.

The gateway is the one door: it wakes a managed machine when work arrives and
powers it off again when the machine has been idle. This client never wakes a
machine by itself; only `sb ai hold --wake` asks the gateway to.

Standard library only. The gateway key is read from the environment or from
the config file, sent only in the Authorization header, and never printed.
"""

import argparse
import json
import math
import os
import random
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

try:
    import ai_fit
    import ai_tune
except ImportError:  # the module sits next to this file
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import ai_fit
    import ai_tune

OK, GATEWAY_ERROR, USAGE, UNREACHABLE = 0, 1, 2, 3
MACHINE_OFF = 3  # the same code as an unreachable gateway: the work cannot start

HELP = """\
sb ai: images, video, audio and machine status through a Switchback gateway

  sb ai image "prompt" [--model NAME] [--aspect W:H] [--mp N] [--size WxH] [--seed N] [-o FILE]
  sb ai video "prompt" --image FIRST_FRAME [--model NAME] [--seconds N] [--frames N]
                       [--aspect W:H] [--mp N] [--size WxH] [--seed N] [-o FILE]
  sb ai bench [MODEL ...] [--workload chat|read|all] [--runs N] [--wake] [--keep-cache]
              [--json] [-o FILE]
                                 measure the text models: first token, prompt and decode speed
  sb ai status [--json]          lanes and machine power; never wakes anything
  sb ai models [--all] [--json]  what the gateway offers, by job (text, image, video,
                                 segment, audio); --all also shows internal provider ids
  sb ai hold DURATION [REASON] [--wake] [--host NAME] [--load MODEL]
                                 keep the machine on for direct work (30m, 2h, 1h30m)
  sb ai release [ID | --all] [--host NAME]
                                 let the machine go again
  sb ai on [DURATION] [REASON] [--host NAME]
                                 keep the machine on and wake it (1h by default)
  sb ai off [--release-holds] [--host NAME]
                                 power the machine off now
  sb ai segment IMAGE ["what to segment"] [--model ID] [-o FILE]
                                 segment an image; the mask is saved next to it
  sb ai transcribe FILE [--model ID] [--language CODE] [--speakers]
                   [--format text|srt|vtt|json] [-o OUT]
                                 speech to text; --speakers says who spoke when
  sb ai speak "TEXT" [--model ID] [--voice V] -o OUT
                                 text to speech, saved to OUT
  sb ai fit REPO [--ctx N] [--machines FILE] [--vram GB] [--ram GB]
                 [--bandwidth GBS] [--json]
  sb ai fit REPO --store TARGET [--quant LABEL] [--bandwidth GBS]
                                  write the target's ceiling (bytes per token, and the
                                  decode ceiling when the bandwidth is known)
  sb ai doctor [MODEL] [--json]   the speed report: measured speed against the ceiling,
                                  swaps, cache hits, acceptance, findings and what to do
  sb ai tune MODEL [--levers LIST] [--budget 30m] [--wake] [--workload chat|read|mixed]
                  [--concurrency 1,2,3] [--samples N] [--dry-run] [--history] [--json]
                                  controlled A/B/A speed experiments on one local model;
                                  keeps only what measurably wins (`sb ai revert` undoes it)
                                 where a model fits and which file to take
  sb ai settings MODEL [--profile NAME] [--history] [--json]
                                 a model's settings: each key, its value and where it comes from
  sb ai set MODEL key=value ... [--profile NAME] [--lock]
                                 change request settings (temperature=0.6 reasoning.enabled=false)
  sb ai unset MODEL key ... [--profile NAME]
                                 remove settings (and their locks)
  sb ai revert MODEL REVISION    put back the settings of an earlier revision
  sb ai set MODEL key=value ... --load [--apply next-load|now] [--wake]
                                 change load settings (ctx_size=65536 cache_type_k=q8_0)
  sb ai chat MODEL "prompt" [--profile NAME] [--set key=value ...] [--load key=value ...]
             [--json]            one quick request to try settings: answer, tokens, speed, route
  sb ai load MODEL [--wake]      load a model now (preload); never wakes without --wake
  sb ai unload MODEL             unload a model; refused while it serves a request
  sb ai engines [--json]         what each engine has loaded, with memory and pending settings

image and video print the saved file path on stdout; progress goes to stderr.
They wait up to --timeout SECS (default 900). hold takes --json for the raw answer.
--model takes the full id or its last part (flux for <lane>/flux or image/flux).
video without --aspect or --size keeps the first frame's shape (0.4 MP, or --mp).
Ctrl-C during a video cancels the job on the gateway.

on is a hold with a wake: the machine comes up and stays on for DURATION
(1h by default) and the answer says how to let it go again; a machine that
already runs is held without a wake, and on says it is up. off powers the
machine off now, through the gateway: it refuses while a hold is active
(unless --release-holds ends it first), while the machine has work, or
while the machine's guard says somebody else is using it, and it says why;
a machine that already sleeps is left alone, and off says it is off.
segment runs a workflow that takes an image (and an optional text) and
gives back a mask; Ctrl-C cancels the job on the gateway, as for video
(during the moment the job is being sent, it says how to stop it instead).

bench measures text models through the gateway's OpenAI-compatible API: it sends a
fixed prompt and reports the time to the first token, the prompt and the decode
speed (tokens per second), the total time and the token counts. The first run of
each model and workload stands alone as cold (it may carry the model load); the
median of the later runs is the warm number. --workload picks chat (a 1000 token
prompt, 256 tokens back) or read (a 9000 token document, 16 tokens back, like the
nightly reading job); --workload all runs both. bench never wakes the machine by
itself: without --wake it says the machine is off and stops (exit 3). With --wake
it holds the machine for 30 minutes and lets it go again at the end, also when a
run fails or you press Ctrl-C. --json prints the numbers as JSON, -o FILE writes
them there too.
Each run starts its prompt with a new line, so the engine cannot reuse the
prompt it read before; --keep-cache sends the same prompt every run instead
(to see what the prompt cache saves).

transcribe sends FILE to the gateway's route `transcribe` (or --model) and prints
the transcript (--format text, the default) or writes it to OUT. --speakers asks
the route `diarize` (or --model) who spoke when and prints one line per turn,
like `[00:01:03] SPEAKER_00: text`; with --format json it gives the raw answer.
A file that is not wav, mp3, m4a, flac, ogg or webm is first turned into 16 kHz
mono Ogg/Opus at 32 kbps (FLAC when ffmpeg has no libopus) when ffmpeg is on the
PATH, else sent as it is. speak saves the audio to OUT; the extension picks the
format (.mp3, .wav, .opus or .ogg, .aac, .flac, .pcm). Its
model is --model, else SWITCHBACK_AI_SPEECH_MODEL, else the gateway's one speech
model. Both wait up to --timeout SECS (default 900).
fit reads the model index, never the model itself, and says where a model
fits and which file to take: the best quant that fits whole, the KV cache it
needs at that context, what has to be offloaded, the disk it takes and a
rough writing speed. REPO is owner/name or a huggingface.co link. --ctx is
the context length of the KV cache (32768 by default, f16). --vram, --ram and
--bandwidth describe one machine: a GPU box when --vram is given (--ram is
then its host memory), a unified memory machine when only --ram is given.
--machines FILE reads a list of them:
{"machines": [{"name": "box", "kind": "gpu", "vram_gb": 24, "ram_gb": 62,
"bandwidth_gbs": 1008}, {"name": "laptop", "kind": "unified",
"usable_gb": 28, "bandwidth_gbs": 546}]}
SB_AI_MACHINES names the same file. Without a machine only the file sizes
and the KV cache are shown. The speed is an estimate: the bandwidth of the
machine times 0.55 divided by the bytes a token reads.

--json prints the same answer with stable keys: repo, kind (gguf,
safetensors, diffusion), context_length, model {architecture, layers,
kv_layers, kv_heads, kv_heads_per_layer, head_dim, value_dim, experts,
experts_used, max_context_length, sliding_window, sliding_layers, source},
kv_cache {bytes, known, element_bytes, note},
options [{quant, files [{name, bytes}], bytes, quality_bits, bytes_per_token,
bytes_per_token_source}], projector {files, chosen, bytes} (chosen: the one
file a server loads), draft {files, bytes},
components [{folder, name, bytes, role}], machines [{name, kind,
budget_bytes, host_bytes, bandwidth_gbs, verdict (fits, partial, no-fit),
quant, files, weights_bytes, kv_cache_bytes, projector_bytes, margin_bytes,
needed_bytes, on_gpu_bytes, offloaded_bytes, offloaded_fraction,
bytes_per_token, speed_estimate_tok_s, disk_bytes, faster_alternative,
warnings}], notes. A diffusion repository answers with the model it takes
and the parts around it instead: model {name, bytes}, on_gpu [], offload [].

settings, set, unset, revert and chat work on a model's settings in the gateway.
MODEL is a public name (a route) or provider/model; set, unset and revert need
one target, so a route over several targets asks for provider/model. Request
settings: temperature, top_p, top_k, min_p, typical_p, repeat_penalty,
repeat_last_n, presence_penalty, frequency_penalty, seed, max_tokens, stop,
system_prompt, system_prompt_mode (prepend, replace_if_absent), response_format,
reasoning.enabled, reasoning.effort (none, low, medium, high),
reasoning.budget_tokens, and extra.KEY for any engine-specific per-request key
(sent as it is). Values are JSON when they parse as JSON: top_k=20,
reasoning.enabled=false, stop='["\\n\\n"]'; anything else is a string.
--profile NAME sets the keys in a named profile instead of the defaults;
--lock also locks them, so a request cannot change them. A request picks a
profile with the header x-switchback-profile: NAME or the body
"switchback": {"profile": NAME, "settings": {...}}. settings shows every key:
its value and its source (defaults, profile:NAME, route_profile:NAME, or
engine when nothing sets it); --profile shows what a request naming that
profile gets; --history lists the changes (who, when, which keys). A change
made meanwhile by someone else stops set with exit 1; run it again.
chat streams one answer to stdout and prints the tokens, the first token time,
the speed and the route on stderr; --set key=value sends a setting for this
request only; --load key=value asks for load settings for this request (a
variant of the model with them, when the model's request_load policy allows
it; the route header names the variant).

Load settings change how the engine loads the model: ctx_size, n_gpu_layers,
cache_type_k, cache_type_v, flash_attn, batch_size, ubatch_size, parallel,
threads, threads_batch, mlock, mmap, load_mode, rope_scaling, rope_freq_base,
rope_freq_scale, yarn_*, spec_type, spec_draft_model, draft_max, draft_min,
mmproj, sleep_idle_seconds, fit, fit_ctx, reasoning_budget, temp, top_k,
top_p, min_p, and engine_args.NAME for any other engine argument (operator
keys). set --load writes them; the gateway hands them to the lane's
settings_command and the engine re-reads its presets. --apply next-load (the
default) leaves a loaded model on its old settings until it loads again;
--apply now reloads it once nothing is in flight on it (exit 1 when it stays
busy). Neither wakes a sleeping machine unless --wake. load preloads a model
(exit 3 when the machine sleeps and --wake is not given); unload never powers
anything off. engines never wakes anything: a sleeping lane shows what it had
loaded when it was last seen. hold --load MODEL loads the model once the
machine is up.

Config: SWITCHBACK_AI_URL and SWITCHBACK_AI_KEY in the environment, else in
${XDG_CONFIG_HOME:-~/.config}/switchback/ai.env (KEY=VALUE lines). Optional there too:
SWITCHBACK_AI_IMAGE_MODEL, SWITCHBACK_AI_VIDEO_MODEL, SWITCHBACK_AI_SEGMENT_MODEL,
SWITCHBACK_AI_SPEECH_MODEL (defaults), SWITCHBACK_AI_OUT_DIR.
Exit codes: 0 ok, 1 the gateway reported an error, 2 usage error, 3 the gateway does
not answer, or the machine is off. fit answers 3 when the model index cannot be
reached and 1 when it answers with an error.
"""

CONFIG_KEYS = ("SWITCHBACK_AI_URL", "SWITCHBACK_AI_KEY", "SWITCHBACK_AI_IMAGE_MODEL",
               "SWITCHBACK_AI_VIDEO_MODEL", "SWITCHBACK_AI_SEGMENT_MODEL",
               "SWITCHBACK_AI_SPEECH_MODEL", "SWITCHBACK_AI_OUT_DIR")
IMAGE_KIND, VIDEO_KIND = "image_generation", "video_generation"
SEGMENT_KIND = "workflow_execution"
# The kinds of work the gateway offers, each with its own models.
MODEL_GROUPS = ("image", "video", "segment", "audio")
# /v1/models marks speech-to-text and text-to-speech models with these kinds.
AUDIO_KINDS = ("audio_transcription", "audio_speech")
# Audio the gateway's engines read as they are; anything else goes through ffmpeg.
AUDIO_EXTENSIONS = {".wav": "audio/wav", ".mp3": "audio/mpeg", ".m4a": "audio/mp4",
                    ".flac": "audio/flac", ".ogg": "audio/ogg", ".webm": "audio/webm"}
# `speak -o FILE`: the extension picks one of OpenAI's speech formats. Opus
# comes in an Ogg container, so .ogg is opus too.
SPEECH_FORMATS = {".mp3": "mp3", ".wav": "wav", ".opus": "opus", ".ogg": "opus", ".aac": "aac",
                  ".flac": "flac", ".pcm": "pcm"}
VIDEO_FPS = 24
# How long `sb ai on` keeps the machine on when no duration is given.
DEFAULT_HOLD_DURATION = "1h"
POLL_SECS = float(os.environ.get("SB_AI_POLL_SECS") or 2.0)  # tests shorten it
VIDEO_MP = 0.4
LANE_WORDS = {"offline": "off", "waking": "waking", "healthy": "idle", "draining": "busy",
              "idle_countdown": "idle", "powering_off": "powering off",
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

    def __init__(self, status, message, has_body, reason="", error_type=""):
        super().__init__(message)
        self.status = status
        self.message = message
        self.has_body = has_body  # False for a bare 404: the route does not exist
        # The gateway's typed reason, when it gave one (a refused power-off
        # says why: `hold_active`, `guard_busy`, ...).
        self.reason = reason
        # The error's `type` (`model_paused`: retry later).
        self.error_type = error_type


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

    def call(self, method, path, body=None, content_type=None, timeout=30, raw=False,
             headers=None):
        extra, headers = headers or {}, {"Accept": "application/json"}
        headers.update(extra)
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
        message, has_body, reason, error_type = "", False, "", ""
        try:
            parsed = json.loads(payload)
            if isinstance(parsed, dict):
                inner = parsed.get("error")
                message = inner.get("message", "") if isinstance(inner, dict) else str(inner or "")
                if isinstance(inner, dict):
                    reason = str(inner.get("reason") or "")
                    error_type = str(inner.get("type") or "")
                has_body = True
        except ValueError:
            pass
        return GatewayError(error.code, self.scrub(message or error.reason or "error"), has_body,
                            reason, error_type)

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

    def stream(self, path, body, timeout=900, headers=None, answer_headers=None):
        """Yields (monotonic_time, chunk) for every chunk of a streaming answer.

        The chunks come as server-sent events; a gateway that ignores
        stream=true answers with one JSON body, which is yielded as one chunk.
        `answer_headers`, a dict, receives the answer's headers (lower-case).
        """
        extra = headers or {}
        headers = {"Accept": "text/event-stream", "Content-Type": "application/json"}
        headers.update(extra)
        if self.key:
            headers["Authorization"] = f"Bearer {self.key}"
        if body is not None and not isinstance(body, bytes):
            body = json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=body, headers=headers, method="POST")
        plain, streamed = [], False
        try:
            with self.opener.open(request, timeout=timeout) as response:
                if answer_headers is not None:
                    answer_headers.update({k.lower(): v for k, v in response.headers.items()})
                for raw in response:
                    line = raw.decode("utf-8", "replace").strip()
                    if not line:
                        continue
                    if not line.startswith("data:"):
                        plain.append(line)
                        continue
                    data = line[5:].strip()
                    if not data:
                        continue
                    if data == "[DONE]":
                        break
                    try:
                        chunk = json.loads(data)
                    except ValueError:
                        continue
                    streamed = True
                    yield time.monotonic(), chunk
        except urllib.error.HTTPError as error:
            raise self._http_error(error) from None
        except urllib.error.URLError as error:
            raise Fail(UNREACHABLE, f"the gateway does not answer ({self.scrub(reason_of(error))})") from None
        except (TimeoutError, socket.timeout):
            raise Fail(GATEWAY_ERROR, f"no answer from the gateway after {timeout:.0f}s") from None
        except OSError as error:
            raise Fail(UNREACHABLE, f"the gateway connection broke ({self.scrub(reason_of(error))})") from None
        if not streamed and plain:
            try:
                chunk = json.loads("\n".join(plain))
            except ValueError:
                return
            yield time.monotonic(), chunk


def reason_of(error):
    return getattr(error, "reason", None) or error


def log(message):
    print(message, file=sys.stderr, flush=True)


def gateway_fail(error, doing):
    return Fail(GATEWAY_ERROR, f"{doing}: the gateway said {error.status}: {error.message}")


# ---------------------------------------------------------------- models

def catalog(gw, every_id=False):
    """{"text": [...], "image": [...], "video": [...], "segment": [...], "audio": [...], ...}."""
    result = {"text": [], "image": [], "video": [], "segment": [], "audio": [], "listed": False,
              "test": set(), "speech": set()}
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
    kinds = {IMAGE_KIND: "image", VIDEO_KIND: "video", SEGMENT_KIND: "segment"}
    for row in workflows.get("data") or []:
        group = kinds.get(row.get("kind"))
        if group and row.get("id") and row["id"] not in result[group]:
            result[group].append(row["id"])
            if str(row.get("provider", "")).endswith("-mock"):
                result["test"].add(row["id"])
    media = set(result["image"]) | set(result["video"]) | set(result["segment"])
    media_providers = {str(row.get("provider")) for row in workflows.get("data") or [] if row.get("provider")}
    for row in models.get("data") or []:
        model_id = row.get("id") if isinstance(row, dict) else None
        if not model_id or model_id in media or model_id in media_providers:
            continue  # a media provider's bare id is listed as a model, but it is not one
        if row.get("kind") in AUDIO_KINDS:
            result["audio"].append(model_id)
            if row["kind"] == "audio_speech":
                result["speech"].add(model_id)
            continue
        prefix = model_id.split("/", 1)[0]
        if prefix in MODEL_GROUPS and "/" in model_id:
            result[prefix].append(model_id)
            media.add(model_id)
        else:
            result["text"].append(model_id)
    if not every_id:
        result["text"] = plain_text_names(result["text"])
        result["audio"] = plain_text_names(result["audio"])
        # /v1/models marks a speech route's targets and provider as speech
        # too; only the names people use are speech models to choose from.
        result["speech"] = {m for m in result["audio"] if m in result["speech"]}
    return result


def plain_text_names(ids):
    """/v1/models lists each text route (`fast`), each provider's own model
    names (`<provider>/<engine name>`) and the provider itself (`<provider>`).
    People use the route names; the rest are internal and repeat them. Drop
    `<provider>/x` when `x` or the provider is listed too, and a bare id that
    only prefixes other ids. `sb ai models --all` shows every id."""
    listed = set(ids)
    prefixes = {i.split("/", 1)[0] for i in ids if "/" in i}
    keep = []
    for model_id in ids:
        if "/" in model_id:
            provider, name = model_id.split("/", 1)
            if name in listed or provider in listed:
                continue
        elif model_id in prefixes:
            continue
        keep.append(model_id)
    return keep


def resolve_model(name, group, cat, default=""):
    others = [g for g in MODEL_GROUPS if g != group]
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
    for other in others:
        if name in cat[other] or any(c.rsplit("/", 1)[-1] == name for c in cat[other]):
            raise Fail(USAGE, f"--model {name} is a {other} model, not a {group} model")
    if not cat["listed"]:
        raise Fail(USAGE, f"this key cannot list the gateway's {group} models; give the full id "
                          f"(like <lane>/{name})")
    if name.startswith(f"{group}/"):
        # A full id the list does not show, such as an old name kept as an alias:
        # aliases resolve at the gateway but are never listed. The gateway decides.
        return name
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


def until_words(expires_ms):
    """Local clock time of an expiry, with the date when it is not today."""
    when = time.localtime(expires_ms / 1000)
    if time.strftime("%Y-%m-%d", when) == time.strftime("%Y-%m-%d"):
        return time.strftime("%H:%M", when)
    return time.strftime("%b %d %H:%M", when)


def holds_words(holds):
    """The holds of a machine: id, reason, owner, until when, minutes left."""
    if not holds:
        return ""
    parts = []
    for hold in holds:
        bits = [str(hold.get("id", "?"))]
        if hold.get("reason"):
            bits.append(f"for {hold['reason']}")
        if hold.get("owner"):
            bits.append(f"by {hold['owner']}")
        if hold.get("state") == "waking":
            bits.append("(waking the machine)")
        elif hold.get("state") == "wake_failed":
            bits.append(f"(wake failed: {hold.get('wake_detail') or 'no detail'})")
        if isinstance(hold.get("expires_at_ms"), (int, float)):
            left = max(0, math.ceil((hold["expires_at_ms"] / 1000 - time.time()) / 60))
            bits.append(f"until {until_words(hold['expires_at_ms'])}, {left} min left")
        parts.append(" ".join(bits))
    return f"{len(holds)} hold{'s' if len(holds) != 1 else ''} ({'; '.join(parts)})"


def machine_holds(lanes):
    """{machine: [hold, ...]}: each lane lists its machine's holds; one copy of each."""
    found = {}
    for lane in lanes:
        held = found.setdefault(machine_of(lane), [])
        for hold in lane.get("holds") or []:
            if not any(hold.get("id") == seen.get("id") for seen in held):
                held.append(hold)
    return found


def lane_line(lane, holds=None):
    """One lane in plain words. `holds` are its machine's holds (else the lane's own)."""
    state = lane.get("state", "?")
    holds = holds if holds is not None else lane.get("holds") or []
    words = LANE_WORDS.get(state, state)
    parts = [f"{words}, held: will not power off" if holds else words,
             f"level {lane.get('level', '?')}", f"queue {lane.get('queue_depth', 0)}"]
    if lane.get("in_flight"):
        parts.append(f"running {lane['in_flight']}")
    if lane.get("holds_gpu") is not None:
        parts.append("has the GPU" if lane["holds_gpu"] else "GPU with another lane")
    # A countdown only when the gateway gives one and nothing holds the
    # machine; null means no countdown runs (work, a hold, off, no power-off).
    countdown = lane.get("poweroff_in_secs")
    if not holds and isinstance(countdown, (int, float)) and not isinstance(countdown, bool):
        parts.append(f"powers off in {duration_words(countdown)}" if countdown > 0 else "power-off due")
    if lane.get("poweroff_guard_busy"):
        parts.append("power-off postponed: the machine reports another user")
    if holds:
        parts.append(holds_words(holds))
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
    held = machine_holds(lanes)
    for machine in machines(lanes):
        print(f"{machine['machine']}: {machine['power']}")
        for lane in lanes:
            if machine_of(lane) == machine["machine"]:
                print(lane_line(lane, held[machine["machine"]]))
    return OK


def cmd_models(gw, args):
    cat = catalog(gw, every_id=args.all)
    groups = {name: cat[name] for name in ("text",) + MODEL_GROUPS}
    if args.json:
        print(json.dumps(groups, indent=2))
        return OK
    for name, ids in groups.items():
        print(f"{name}:")
        for model_id in ids:
            note = " (test)" if model_id in cat["test"] else ""
            note += " (speech)" if model_id in cat["speech"] else ""
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


def frame_size(data):
    """(width, height) from an image header, as it shows upright; None if unreadable.

    PNG IHDR; JPEG SOF0/1/2 (an Exif orientation of 5 to 8 turns it a quarter);
    WebP VP8, VP8L and VP8X."""
    size = None
    if data[:8] == b"\x89PNG\r\n\x1a\n" and data[12:16] == b"IHDR" and len(data) >= 24:
        size = int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")
    elif data[:3] == b"\xff\xd8\xff":
        size = jpeg_size(data)
    elif data[:4] == b"RIFF" and data[8:12] == b"WEBP" and len(data) >= 30:
        chunk = data[12:16]
        if chunk == b"VP8 " and data[23:26] == b"\x9d\x01\x2a":
            size = (int.from_bytes(data[26:28], "little") & 0x3FFF,
                    int.from_bytes(data[28:30], "little") & 0x3FFF)
        elif chunk == b"VP8L" and data[20] == 0x2F:
            bits = int.from_bytes(data[21:25], "little")
            size = (bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1
        elif chunk == b"VP8X":
            size = int.from_bytes(data[24:27], "little") + 1, int.from_bytes(data[27:30], "little") + 1
    if size and size[0] > 0 and size[1] > 0:
        return size
    return None


def jpeg_size(data):
    i, turned = 2, False
    while i + 4 <= len(data):
        if data[i] != 0xFF:
            return None
        marker = data[i + 1]
        if marker == 0xFF:  # fill byte
            i += 1
            continue
        if marker == 0x01 or 0xD0 <= marker <= 0xD7:  # markers without a length
            i += 2
            continue
        if marker in (0xD9, 0xDA):  # end, or image data before any frame header
            return None
        length = int.from_bytes(data[i + 2:i + 4], "big")
        if length < 2:
            return None
        segment = data[i + 4:i + 2 + length]
        if marker in (0xC0, 0xC1, 0xC2):
            if len(segment) < 5:
                return None
            height, width = int.from_bytes(segment[1:3], "big"), int.from_bytes(segment[3:5], "big")
            return (height, width) if turned else (width, height)
        if marker == 0xE1 and segment[:6] == b"Exif\x00\x00":
            turned = exif_orientation(segment[6:]) in (5, 6, 7, 8)
        i += 2 + length
    return None


def exif_orientation(tiff):
    """The Orientation tag (0x0112) of IFD0 in an Exif TIFF block; 1 if absent."""
    order = {b"II": "little", b"MM": "big"}.get(tiff[:2])
    if not order or len(tiff) < 8:
        return 1
    ifd = int.from_bytes(tiff[4:8], order)
    if ifd + 2 > len(tiff):
        return 1
    count = int.from_bytes(tiff[ifd:ifd + 2], order)
    entries = (tiff[ifd + 2 + 12 * k:ifd + 14 + 12 * k] for k in range(count))
    for entry in entries:
        if len(entry) < 12:
            break
        if int.from_bytes(entry[0:2], order) == 0x0112:
            return int.from_bytes(entry[8:10], order)
    return 1


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
    if args.aspect:
        size = size_for(parse_aspect(args.aspect), args.mp or VIDEO_MP)
    try:
        with open(args.image, "rb") as handle:
            frame = handle.read()
    except OSError as error:
        raise Fail(USAGE, f"cannot read --image {args.image}: {error.strerror}")
    media_type = image_type(frame)
    if not media_type:
        raise Fail(USAGE, "--image must be a PNG, JPEG or WebP file")
    model = resolve_model(args.model, "video", catalog(gw), config["SWITCHBACK_AI_VIDEO_MODEL"])
    if not (args.size or args.aspect):
        # No shape asked for: keep the first frame's (at --mp, if given), else
        # the workflow renders its own default (often square).
        mp = args.mp or VIDEO_MP
        shape = frame_size(frame)
        if shape:
            size = size_for(shape[0] / shape[1], mp)
            log(f"first frame is {shape[0]}x{shape[1]}; sending size {size} (same shape, {mp:g} MP)")
        elif args.mp is not None:
            size = size_for(1.0, mp)
            log(f"cannot read the first frame's width and height; sending a square size {size} at {mp:g} MP")
        else:
            log("cannot read the first frame's width and height; sending no size, so the workflow "
                "uses its default (give --aspect or --size to choose)")
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
    started, job_id, submitting = time.time(), None, False
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
        submitting = True  # from here the gateway may have the job
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
        return interrupted(gw, job_id, submitting)
    videos = [a for a in job.get("artifacts") or []
              if a.get("kind") == "video" or str(a.get("media_type", "")).startswith("video/")]
    if not videos:
        raise Fail(GATEWAY_ERROR, f"job {job_id} succeeded without a video")
    log(f"job {job_id} done in {time.time() - started:.0f}s")
    tag = str(args.seed) if args.seed is not None else str(job_id)[-8:]
    for index, video in enumerate(videos):
        save_artifact(gw, video, output_path(args, config, model, tag, index, video.get("media_type")), args.timeout)
    return OK


def interrupted(gw, job_id, submitting):
    """Ctrl-C during a job: cancel it on the gateway when its id is known.

    Between sending the job and reading its answer the id is not known yet,
    and the gateway may already run it: say so, and how to stop it, rather
    than guess which job is ours."""
    if job_id:
        try:
            gw.post(f"/v1/jobs/{urllib.parse.quote(job_id, safe='')}/cancel", {}, timeout=20)
            log(f"\ncancelled job {job_id} on the gateway")
        except (Fail, GatewayError) as error:
            log(f"\ninterrupted; cancelling job {job_id} failed ({getattr(error, 'message', error)})")
    elif submitting:
        log("\ninterrupted while the job was being sent: the gateway may still run it. "
            "Find it with GET /v1/jobs and stop it with POST /v1/jobs/ID/cancel")
    else:
        log("\ninterrupted before the job started")
    return 130


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


# ---------------------------------------------------------------- segment

def cmd_segment(gw, config, args):
    """Segment an image: upload it, run the workflow, save the mask."""
    try:
        with open(args.image, "rb") as handle:
            picture = handle.read()
    except OSError as error:
        raise Fail(USAGE, f"cannot read {args.image}: {error.strerror}")
    media_type = image_type(picture)
    if not media_type:
        raise Fail(USAGE, f"{args.image} must be a PNG, JPEG or WebP file")
    model = resolve_model(args.model, "segment", catalog(gw), config["SWITCHBACK_AI_SEGMENT_MODEL"])
    inputs = {}
    if args.text:
        inputs["prompt"] = args.text
    note_machine(gw)
    started, job_id, submitting = time.time(), None, False
    try:
        try:
            upload = gw.post("/v1/uploads", picture, content_type=media_type, timeout=120)
        except GatewayError as error:
            raise gateway_fail(error, "the image was refused")
        except Fail as failure:
            if failure.code == UNREACHABLE and gw.answers():
                raise Fail(GATEWAY_ERROR, f"the upload broke off ({len(picture) / 2 ** 20:.1f} MiB); the "
                                          "gateway may refuse files above its upload limit") from None
            raise
        inputs["image"] = upload["artifact_id"]
        log(f"image uploaded ({len(picture) // 1024} KB)")
        submitting = True  # from here the gateway may have the job
        try:
            job = gw.post("/v1/jobs", {"kind": SEGMENT_KIND, "model": model, "inputs": inputs})
        except GatewayError as error:
            raise gateway_fail(error, "the segmentation was refused")
        job_id = job["id"]
        log(f"segmentation job {job_id} started with {model}"
            + (f" ({inputs['prompt']})" if "prompt" in inputs else ""))
        job = poll(gw, job, args.timeout, started)
    except KeyboardInterrupt:
        return interrupted(gw, job_id, submitting)
    masks = [a for a in job.get("artifacts") or []
             if a.get("kind") == "image" or str(a.get("media_type", "")).startswith("image/")]
    if not masks:
        raise Fail(GATEWAY_ERROR, f"job {job_id} succeeded without a mask")
    log(f"job {job_id} done in {time.time() - started:.0f}s")
    tag = str(job_id)[-8:]
    for index, mask in enumerate(masks):
        save_artifact(gw, mask,
                      output_path(args, config, model, tag, index, mask.get("media_type")),
                      args.timeout)
    return OK


# ---------------------------------------------------------------- audio

def multipart(fields, filename, content_type, data):
    """A multipart/form-data body: the text fields, then the file."""
    boundary = f"sb-ai-{uuid.uuid4().hex}"
    parts = []
    for name, value in fields:
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n'
                     f"{value}\r\n".encode())
    safe = filename.replace('"', "'").replace("\r", " ").replace("\n", " ")
    parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{safe}"\r\n'
                 f"Content-Type: {content_type}\r\n\r\n".encode())
    parts.append(data)
    parts.append(f"\r\n--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def ffmpeg_has_libopus(ffmpeg):
    run = subprocess.run([ffmpeg, "-hide_banner", "-encoders"], capture_output=True, text=True)
    return run.returncode == 0 and "libopus" in run.stdout


def audio_file(path, workdir):
    """(name, content type, bytes) to send: the file itself when engines read
    it as it is, else mono 16 kHz Ogg/Opus at 32 kbps made by ffmpeg when it is
    installed (about 14 MiB an hour, well under the usual 25 MiB upload limit),
    or FLAC when this ffmpeg has no libopus."""
    if not os.path.isfile(path):
        raise Fail(USAGE, f"cannot read {path}: no such file")
    name = os.path.basename(path)
    ext = os.path.splitext(name)[1].lower()
    ffmpeg = shutil.which("ffmpeg")
    if ext not in AUDIO_EXTENSIONS and ffmpeg:
        if ffmpeg_has_libopus(ffmpeg):
            ext, label, codec = ".ogg", "Ogg/Opus", ["-c:a", "libopus", "-b:a", "32k", "-f", "ogg"]
        else:
            ext, label, codec = ".flac", "FLAC", ["-c:a", "flac", "-f", "flac"]
        converted = os.path.join(workdir, os.path.splitext(name)[0] + ext)
        log(f"converting {name} to 16 kHz mono {label}")
        # `file:` keeps a name like `concat:a|b` or `http://x` a plain file.
        source = "file:" + os.path.abspath(path)
        run = subprocess.run([ffmpeg, "-nostdin", "-loglevel", "error", "-y", "-i", source,
                              "-vn", "-ac", "1", "-ar", "16000", *codec, converted],
                             capture_output=True, text=True)
        if run.returncode != 0:
            detail = (run.stderr.strip().splitlines() or ["no detail"])[-1]
            raise Fail(USAGE, f"ffmpeg cannot read {path}: {detail}")
        path, name = converted, os.path.basename(converted)
    try:
        with open(path, "rb") as handle:
            data = handle.read()
    except OSError as error:
        raise Fail(USAGE, f"cannot read {path}: {error.strerror}")
    return name, AUDIO_EXTENSIONS.get(ext, "application/octet-stream"), data


def clock(seconds):
    seconds = max(0, int(seconds or 0))
    return f"{seconds // 3600:02d}:{seconds % 3600 // 60:02d}:{seconds % 60:02d}"


def speaker_turns(answer):
    """`[00:01:03] SPEAKER_00: text` lines, one per turn: segments in a row
    from the same speaker are one turn."""
    turns = []
    for segment in answer.get("segments") or []:
        text = str(segment.get("text") or "").strip()
        speaker = str(segment.get("speaker") or "SPEAKER")
        if not text:
            continue
        if turns and turns[-1][1] == speaker:
            turns[-1][2].append(text)
        else:
            turns.append([segment.get("start") or 0, speaker, [text]])
    if not turns and answer.get("text"):
        return [str(answer["text"]).strip()]
    return [f"[{clock(start)}] {speaker}: {' '.join(texts)}" for start, speaker, texts in turns]


def write_output(path, data):
    path = os.path.abspath(path)
    part = path + ".part"
    try:
        with open(part, "wb") as handle:
            handle.write(data)
        os.replace(part, path)
    except OSError as error:
        raise Fail(USAGE, f"cannot write {path}: {error.strerror}") from None
    print(path, flush=True)


def cmd_transcribe(gw, args):
    fmt = args.format or "text"
    if args.speakers and args.format in ("srt", "vtt"):
        raise Fail(USAGE, "--speakers gives turns or json, not srt or vtt")
    model = args.model or ("diarize" if args.speakers else "transcribe")
    response_format = "diarized_json" if args.speakers else fmt
    fields = [("model", model), ("response_format", response_format)]
    if args.language:
        fields.append(("language", args.language))
    with tempfile.TemporaryDirectory(prefix="sb-ai-audio-") as workdir:
        name, content_type, data = audio_file(args.file, workdir)
        body, body_type = multipart(fields, name, content_type, data)
    log(f"transcribing {name} ({len(data) / 2 ** 20:.1f} MiB) with {model}")
    started = time.time()
    try:
        answer = gw.call("POST", "/v1/audio/transcriptions", body=body, content_type=body_type,
                         timeout=args.timeout, raw=True)
    except GatewayError as error:
        if error.status == 413:
            raise Fail(GATEWAY_ERROR, f"the file is too large for the gateway: {error.message}")
        raise gateway_fail(error, "the transcription failed")
    except Fail as failure:
        if failure.code == UNREACHABLE and gw.answers():
            raise Fail(GATEWAY_ERROR, f"the upload broke off ({len(data) / 2 ** 20:.1f} MiB); the "
                                      "gateway may refuse files above its upload limit") from None
        raise
    log(f"done in {time.time() - started:.0f}s")
    if args.speakers and fmt != "json":
        try:
            parsed = json.loads(answer)
        except ValueError:
            raise Fail(GATEWAY_ERROR, "the gateway sent speaker turns that are not JSON") from None
        text = "\n".join(speaker_turns(parsed)) + "\n"
        output = text.encode()
    elif fmt == "json":
        output = answer if answer.endswith(b"\n") else answer + b"\n"
    else:
        output = answer if answer.endswith(b"\n") or not answer else answer + b"\n"
    if args.output:
        write_output(args.output, output)
    else:
        sys.stdout.write(output.decode("utf-8", "replace"))
        sys.stdout.flush()
    return OK


def speech_model(gw, config, args):
    if args.model or config["SWITCHBACK_AI_SPEECH_MODEL"]:
        return args.model or config["SWITCHBACK_AI_SPEECH_MODEL"]
    speech = sorted(catalog(gw)["speech"])
    if len(speech) == 1:
        return speech[0]
    listed = ", ".join(speech) or "none"
    raise Fail(USAGE, f"choose a speech model with --model (the gateway offers: {listed})")


def cmd_speak(gw, config, args):
    if not args.text.strip():
        raise Fail(USAGE, "give the text to speak")
    fmt = SPEECH_FORMATS.get(os.path.splitext(args.output)[1].lower())
    if not fmt:
        allowed = ", ".join(SPEECH_FORMATS)
        raise Fail(USAGE, f"-o must end in one of {allowed}: the extension picks the audio format")
    model = speech_model(gw, config, args)
    body = {"model": model, "input": args.text, "response_format": fmt}
    if args.voice:
        body["voice"] = args.voice
    log(f"speaking {len(args.text)} characters with {model}")
    try:
        audio = gw.call("POST", "/v1/audio/speech", body=body, timeout=args.timeout, raw=True)
    except GatewayError as error:
        raise gateway_fail(error, "the speech failed")
    if not audio:
        raise Fail(GATEWAY_ERROR, "the gateway sent no audio")
    write_output(args.output, audio)
    return OK


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
    # A lane with a machine of its own has no `host` in the full report; its
    # machine is then named after the lane (the holds API takes that name).
    lanes = (capacity(gw).get("data") or [])
    hosts = sorted({machine_of(lane) for lane in lanes})
    if not hosts:
        raise Fail(GATEWAY_ERROR, "the gateway manages no machine (no local lanes)")
    host = only_host(lanes)
    if host:
        return host
    raise Fail(USAGE, f"the gateway manages several machines ({', '.join(hosts)}); choose one with --host")


def only_host(lanes):
    """The one machine a bare hold or model means, or None.

    With several machines, the one the gateway powers on and off wins when it is
    the only such machine: an always-on machine (no wake, no power-off) needs no
    hold, so `sb ai hold 2h --wake` keeps meaning the machine that sleeps."""
    hosts = sorted({machine_of(lane) for lane in lanes})
    if len(hosts) == 1:
        return hosts[0]
    managed = sorted({machine_of(lane) for lane in lanes
                      if lane.get("wake_configured") or lane.get("poweroff_configured")})
    return managed[0] if len(managed) == 1 else None


def holds_path(host, hold_id=None):
    path = f"/v1/hosts/{urllib.parse.quote(host, safe='')}/holds"
    return path + (f"/{urllib.parse.quote(hold_id, safe='')}" if hold_id else "")


def holds_error(error, doing):
    if error.status == 404 and not error.has_body:
        return Fail(GATEWAY_ERROR, "this gateway has no holds yet")
    return gateway_fail(error, doing)


def create_hold(gw, host, ttl_secs, reason, wake=False, load=None):
    """Ask the gateway to hold a machine on; returns the hold it answers with."""
    body = {"ttl_secs": ttl_secs, "reason": reason, "wake": bool(wake)}
    if load:
        body["load"] = load
    try:
        return gw.post(holds_path(host), body)  # 201 with the hold itself
    except GatewayError as error:
        raise holds_error(error, "the hold was refused")


def release_hold(gw, host, hold_id):
    """Let a hold go; the machine may power off again."""
    try:
        gw.delete(holds_path(host, hold_id))
    except GatewayError as error:
        raise holds_error(error, f"releasing hold {hold_id}")


def cmd_hold(gw, args):
    ttl = parse_duration(args.duration)
    host = host_for(gw, args)
    hold = create_hold(gw, host, ttl, args.reason or "sb ai hold", bool(args.wake),
                       getattr(args, "load", None))
    if args.json:
        print(json.dumps(hold, indent=2))
        return OK
    woke = "; the gateway wakes it" if hold.get("state") == "waking" else ""
    print(f"holding {host} on for {duration_words(ttl)}: hold {hold.get('id', '?')}{woke}")
    if isinstance(hold.get("load"), dict):
        print(f"loading {hold['load'].get('target')}: {hold['load'].get('status')}")
    print(f"let it go: sb ai release {hold.get('id', '')}".rstrip())
    return OK


def cmd_release(gw, args):
    if args.all and args.id:
        raise Fail(USAGE, "give a hold ID or --all, not both")
    host = host_for(gw, args)
    ids = [args.id] if args.id else None
    if ids is None:
        try:
            holds = gw.get(holds_path(host)).get("data") or []
        except GatewayError as error:
            raise holds_error(error, "listing holds")
        ids = [str(h["id"]) for h in holds if h.get("id")]
        if not ids:
            print(f"{host} has no holds")
            return OK
        if len(ids) > 1 and not args.all:
            raise Fail(USAGE, f"{host} has {len(ids)} holds ({', '.join(ids)}); give an ID or --all")
    for hold_id in ids:
        release_hold(gw, host, hold_id)
        print(f"released hold {hold_id} on {host}")
    return OK


# ---------------------------------------------------------------- power

def power_path(host):
    return f"/v1/hosts/{urllib.parse.quote(host, safe='')}/power"


def machine_up(gw, host):
    """Ask the gateway to wake `host`: True when it answers that the machine
    is up (no wake sent), False when the wake started, None when this key
    may not use the power endpoint or the gateway has none."""
    try:
        answer = gw.post(power_path(host), {"action": "on"})
    except GatewayError:
        return None
    return answer.get("status") == "up"


def cmd_on(gw, args):
    """Keep the machine on and wake it: a hold with a wake."""
    if not args.duration:
        args.duration = DEFAULT_HOLD_DURATION
    if not args.reason:
        args.reason = "sb ai on"
    ttl = parse_duration(args.duration)
    host = host_for(gw, args)
    up = machine_up(gw, host)
    hold = create_hold(gw, host, ttl, args.reason, wake=not up)
    if args.json:
        print(json.dumps(hold, indent=2))
        return OK
    state = f"{host} is already up" if up else f"{host} is waking"
    print(f"{state}; holding it on for {duration_words(ttl)}: hold {hold.get('id', '?')}")
    print(f"let it go: sb ai release {hold.get('id', '')}".rstrip())
    return OK


def cmd_off(gw, args):
    """Power the machine off now, through the gateway."""
    host = host_for(gw, args)
    body = {"action": "off"}
    if args.release_holds:
        body["release_holds"] = True
    try:
        answer = gw.post(power_path(host), body)
    except GatewayError as error:
        if error.reason:
            raise Fail(GATEWAY_ERROR,
                       f"the gateway refused to power {host} off ({error.reason}): {error.message}"
                       ) from None
        raise gateway_fail(error, f"powering {host} off") from None
    released = [str(i) for i in answer.get("released_holds") or []]
    if released:
        print(f"released {'hold' if len(released) == 1 else 'holds'} {', '.join(released)} on {host}")
    print(f"{host} is already off" if answer.get("already_off") else f"{host} is powering off")
    return OK


# ---------------------------------------------------------------- bench

BENCH_TTL = 1800          # the hold a bench run keeps (30 minutes)
BENCH_RUNS = 3
BENCH_WAKE_TIMEOUT = 300  # seconds to wait for the machine to come up
BENCH_TIMEOUT = 900       # seconds for one chat request

BENCH_PARAGRAPH = (
    "The switchback gateway sits between a request and the machine that answers it. "
    "It picks the lane that should serve the job, it keeps the machine awake while the "
    "work runs, and it writes down what happened: the tokens that were counted, the "
    "time to the first token, the lane that was chosen and the price that was paid. "
    "Because the gateway is the only door, a tool can be replaced without changing the "
    "way a person works, and a report can be produced without asking the tool to "
    "remember anything. The same record answers two questions at once: what did the "
    "machine do, and what should it do next. A reading job sends a long document and "
    "asks for a short answer; a writing job sends a short instruction and asks for a "
    "long answer. Both look the same from the outside, and both cost less when the "
    "gateway can tell them apart. Every number in a report comes from that record, so "
    "a slow run and a fast run can be compared without guessing why they differ."
)

WORKLOADS = {
    # a chat: about 1000 tokens in, 256 tokens back.
    "chat": {"max_tokens": 256,
             "prompt": "You are a careful assistant. Read the notes below, then answer the "
                       "question in one short paragraph, using only the notes.\n\n"
                       "Notes:\n" + (BENCH_PARAGRAPH + "\n\n") * 4 +
                       "\nQuestion: what does the gateway record, and why does that matter to "
                       "the person doing the work?"},
    # a reading job like the nightly one: about 9000 tokens in, 16 tokens back.
    "read": {"max_tokens": 16,
             "prompt": "Read the document below. Reply with one line about what it is about "
                       "and nothing else.\n\nDocument:\n" + (BENCH_PARAGRAPH + "\n\n") * 38},
}

BENCH_COLUMNS = (("model", "model"), ("workload", "workload"), ("kind", "kind"),
                 ("runs", "runs"), ("ttft ms", "ttft_ms"), ("prompt tok/s", "prompt_tok_s"),
                 ("decode tok/s", "decode_tok_s"), ("total s", "total_s"), ("source", "source"))


def bench_models(cat, names, gw=None):
    """The models to measure: the ones asked for, else the default text model.

    A name the plain list hides (a route named like `<provider>/<model>`, say
    `mac/qwen38-35b`) still counts when the gateway lists that exact id."""
    if not names:
        try:
            return [resolve_model("", "text", cat, "")]
        except Fail:
            listed = ", ".join(cat["text"]) or "none"
            raise Fail(USAGE, f"choose a text model to bench (the gateway offers: {listed}); "
                              "give its name") from None
    every = None
    out = []
    for name in names:
        try:
            out.append(resolve_model(name, "text", cat, ""))
        except Fail:
            if gw is not None and every is None:
                every = catalog(gw, every_id=True)["text"]
            if not every or name not in every:
                raise
            out.append(name)
    return out


def model_host(model, lanes):
    """The machine behind a model: the lane its name points at, else the only one."""
    hosts = sorted({machine_of(lane) for lane in lanes})
    if "/" in model:
        prefix = model.split("/", 1)[0]
        for lane in lanes:
            if lane.get("name") == prefix:
                return machine_of(lane)
    if not hosts:
        raise Fail(GATEWAY_ERROR, "the gateway manages no machine (no local lanes)")
    host = only_host(lanes)
    if host:
        return host
    raise Fail(USAGE, f"the gateway manages several machines ({', '.join(hosts)}); name the "
                      "model with its lane (like lane/model)")


def host_power(lanes, host):
    """The power of one machine, from the states of its lanes."""
    return power_of({lane.get("state") for lane in lanes if machine_of(lane) == host})


def number_of(value):
    """A value as a number, or None when it is not one."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def chunk_has_text(chunk):
    """True when a streamed chunk carries generated text."""
    for choice in chunk.get("choices") or []:
        delta = choice.get("delta") or {}
        # A thinking model sends reasoning before the answer: its first token
        # counts as the first token too.
        if delta.get("content") or delta.get("reasoning_content") or delta.get("reasoning"):
            return True
    return False


def measure(gw, model, workload, keep_cache=False):
    """One chat request, timed: the first token, the usage and the server's timings."""
    work = WORKLOADS[workload]
    prompt = work["prompt"]
    if not keep_cache:
        # A new first line every run, so the engine cannot reuse the prompt it
        # read last time: warm runs measure reading, not the prompt cache.
        prompt = f"Run {uuid.uuid4().hex[:12]}.\n" + prompt
    body = {"model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": work["max_tokens"], "temperature": 0, "seed": 1,
            "reasoning_effort": "none", "stream": True,
            "stream_options": {"include_usage": True}}
    started = time.monotonic()
    first, usage, timings = None, None, None
    for when, chunk in gw.stream("/v1/chat/completions", body, timeout=BENCH_TIMEOUT):
        if timings is None and isinstance(chunk.get("timings"), dict):
            timings = chunk["timings"]
        if isinstance(chunk.get("usage"), dict) and chunk["usage"]:
            usage = chunk["usage"]
        if first is None and chunk_has_text(chunk):
            first = when
    ended = time.monotonic()
    if first is None:
        raise Fail(GATEWAY_ERROR, f"{model} {workload}: the answer carried no token")
    if not usage:
        raise Fail(GATEWAY_ERROR, f"{model} {workload}: the answer carried no usage "
                                  "(the gateway does not report it while streaming)")
    ttft = (first - started) * 1000
    total = ended - started
    decoded = ended - first
    prompt_tokens = int(usage.get("prompt_tokens") or 0)
    completion_tokens = int(usage.get("completion_tokens") or 0)
    prompt_speed = prompt_tokens / (ttft / 1000) if ttft > 0 else 0.0
    decode_speed = completion_tokens / decoded if decoded > 0 else 0.0
    source = "client"
    if timings:
        # llama.cpp times the prompt and the answer itself; its numbers are the
        # ones the engine measured, ours only guess from the wire.
        source = "engine"
        engine_prompt = number_of(timings.get("prompt_per_second"))
        engine_decode = number_of(timings.get("predicted_per_second"))
        if engine_prompt is not None:
            prompt_speed = engine_prompt
        if engine_decode is not None:
            decode_speed = engine_decode
    return {"ttft_ms": ttft, "prompt_tok_s": prompt_speed, "decode_tok_s": decode_speed,
            "total_s": total, "source": source, "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens}


def median(values):
    """The middle of the numbers; the mean of the two middles when there are two."""
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2


def summarize(model, workload, samples):
    """The cold row (the first run alone) and the warm row (the median of the rest)."""
    rows = [dict(samples[0], model=model, workload=workload, kind="cold", runs=1)]
    later = samples[1:]
    if later:
        by_time = sorted(later, key=lambda sample: sample["ttft_ms"])
        rows.append({"model": model, "workload": workload, "kind": "warm", "runs": len(later),
                     "ttft_ms": median([s["ttft_ms"] for s in later]),
                     "prompt_tok_s": median([s["prompt_tok_s"] for s in later]),
                     "decode_tok_s": median([s["decode_tok_s"] for s in later]),
                     "total_s": median([s["total_s"] for s in later]),
                     "source": by_time[len(by_time) // 2]["source"],
                     "prompt_tokens": int(round(median([s["prompt_tokens"] for s in later]))),
                     "completion_tokens": int(round(median([s["completion_tokens"] for s in later])))})
    return rows


def bench_rows(gw, models, workloads, runs, keep_cache=False):
    """Measure every model on every workload: cold first, then the warm median."""
    rows = []
    for model in models:
        for workload in workloads:
            samples = []
            for index in range(runs):
                log(f"{model} {workload}: run {index + 1}/{runs}")
                samples.append(measure(gw, model, workload, keep_cache))
            rows.extend(summarize(model, workload, samples))
    return rows


def bench_cell(row, key):
    """One cell of the table, rounded for reading."""
    if key == "runs":
        return str(row["runs"])
    if key == "ttft_ms":
        return f"{row['ttft_ms']:.1f}"
    if key in ("prompt_tok_s", "decode_tok_s"):
        return f"{row[key]:.1f}"
    if key == "total_s":
        return f"{row['total_s']:.2f}"
    return str(row[key])


def bench_table(rows):
    """The plain table: one line per row, the columns padded to the widest cell."""
    lines = [[label for label, _ in BENCH_COLUMNS]]
    lines += [[bench_cell(row, key) for _, key in BENCH_COLUMNS] for row in rows]
    widths = [max(len(line[index]) for line in lines) for index in range(len(lines[0]))]
    return "\n".join("  ".join(cell.ljust(widths[index]) for index, cell in enumerate(line)).rstrip()
                     for line in lines)


def wait_ready(gw, hosts, timeout=BENCH_WAKE_TIMEOUT):
    """Wait until the machines behind the models are up and their lanes healthy."""
    deadline = time.monotonic() + timeout
    while True:
        lanes = (capacity(gw, quiet=True) or {}).get("data") or []
        powers = {host: host_power(lanes, host) for host in hosts}
        if all(power == "on" for power in powers.values()):
            return
        if time.monotonic() > deadline:
            raise Fail(GATEWAY_ERROR, f"the machine did not come up within {timeout:.0f}s "
                                      f"({', '.join(f'{host}: {power}' for host, power in powers.items())})")
        log("waiting for the machine: " + ", ".join(f"{host}: {power}" for host, power in powers.items()))
        time.sleep(POLL_SECS)


def release_quietly(gw, host, hold_id):
    """Let a hold go, even at the end of a failed run; a failure here is a warning."""
    try:
        release_hold(gw, host, hold_id)
        log(f"released hold {hold_id} on {host}")
    except (Fail, GatewayError) as error:
        log(f"could not release hold {hold_id} on {host}: {getattr(error, 'message', error)}")


def cmd_bench(gw, args):
    if args.runs < 1:
        raise Fail(USAGE, "--runs must be 1 or more")
    workloads = list(WORKLOADS) if args.workload == "all" else [args.workload]
    models = bench_models(catalog(gw), args.model, gw)
    lanes = (capacity(gw).get("data") or [])
    hosts = []
    for model in models:
        host = model_host(model, lanes)
        if host not in hosts:
            hosts.append(host)
    if not args.wake:
        for host in hosts:
            power = host_power(lanes, host)
            if power in ("off", "powering off"):
                raise Fail(MACHINE_OFF, f"{host} is {power}; rerun with --wake to let the gateway "
                                        "wake it, or the bench cannot run")
    holds = []
    try:
        if args.wake:
            for host in hosts:
                hold = create_hold(gw, host, BENCH_TTL, "sb ai bench", True)
                holds.append((host, str(hold.get("id") or "")))
                log(f"holding {host} on for {duration_words(BENCH_TTL)}: hold {hold.get('id', '?')}")
            wait_ready(gw, hosts)
        rows = bench_rows(gw, models, workloads, args.runs, args.keep_cache)
    except GatewayError as error:
        raise gateway_fail(error, "the bench request failed") from None
    finally:
        for host, hold_id in holds:
            release_quietly(gw, host, hold_id)
    report = {"models": models, "workloads": workloads, "runs": args.runs, "results": rows}
    if args.output:
        path = os.path.abspath(args.output)
        try:
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(report, handle, indent=2)
                handle.write("\n")
        except OSError as error:
            raise Fail(USAGE, f"cannot write {path}: {error.strerror}") from None
        log(f"wrote {path}")
    print(json.dumps(report, indent=2) if args.json else bench_table(rows))
    return OK


# ---------------------------------------------------------------- main

# ---------------------------------------------------------------- model settings

# Load-time keys (the gateway's `load` section); the rest are request-time.
LOAD_KEYS = ("ctx_size", "n_gpu_layers", "cache_type_k", "cache_type_v", "flash_attn",
             "batch_size", "ubatch_size", "parallel", "threads", "threads_batch", "mlock",
             "mmap", "load_mode", "rope_scaling", "rope_freq_base", "rope_freq_scale",
             "yarn_orig_ctx", "yarn_ext_factor", "yarn_attn_factor", "yarn_beta_slow",
             "yarn_beta_fast", "spec_type", "spec_draft_model", "draft_max", "draft_min",
             "mmproj", "sleep_idle_seconds", "fit", "fit_ctx", "reasoning_budget", "temp",
             "top_k", "top_p", "min_p")
REQUEST_KEYS = ("temperature", "top_p", "top_k", "min_p", "typical_p", "repeat_penalty",
                "repeat_last_n", "presence_penalty", "frequency_penalty", "seed", "max_tokens",
                "stop", "system_prompt", "system_prompt_mode", "response_format", "reasoning",
                "extra")


# Document-level keys (not request settings): what the speed tricks and the
# speed report read. `reasoning=true` (a boolean) marks a model that thinks;
# `reasoning.enabled=...` stays a request setting.
DOC_KEYS = ("reasoning_guard", "ceiling")


def doc_level(key, value=None, doc=None):
    top = key.partition(".")[0]
    if top in DOC_KEYS:
        return True
    if key == "reasoning":
        return isinstance(value, bool) or (doc is not None and isinstance(doc.get("reasoning"), bool))
    return False


def ceiling_from_fit(report, quant=None, bandwidth=None):
    """The ceiling `sb ai fit --store` writes: bytes per generated token of
    the chosen quant (weights a token reads at short context), and the
    decode ceiling (bandwidth / bytes per token) when a bandwidth is known."""
    options = report.get("options") or []
    if not options:
        raise Fail(USAGE, f"{report.get('repo')} has no model file to take a ceiling from")
    option = None
    if quant:
        option = next((o for o in options if (o.get("quant") or "").lower() == quant.lower()), None)
        if option is None:
            labels = ", ".join(str(o.get("quant")) for o in options)
            raise Fail(USAGE, f"no quant {quant} in {report.get('repo')} ({labels})")
    else:
        chosen = next((m.get("quant") for m in report.get("machines") or [] if m.get("quant")), None)
        option = next((o for o in options if chosen and o.get("quant") == chosen), options[0])
    per_token = option.get("bytes_per_token")
    if not per_token:
        raise Fail(USAGE, f"the bytes per token of {option.get('quant')} are unknown "
                          f"({option.get('bytes_per_token_source')})")
    ceiling = {"bytes_per_token": int(per_token),
               "source": f"sb ai fit {report.get('repo')} {option.get('quant') or ''}".strip()}
    if not bandwidth:
        bandwidth = next((m.get("bandwidth_gbs") for m in report.get("machines") or []
                          if m.get("bandwidth_gbs")), None)
    if bandwidth:
        ceiling["decode_tps"] = round(float(bandwidth) * 1e9 / per_token, 1)
        ceiling["source"] += f" at {float(bandwidth):g} GB/s"
    return ceiling


def cmd_fit_store(gw, args):
    try:
        report = ai_fit.build_report(args, os.environ)
    except ai_fit.FitError as error:
        raise Fail(error.code, error.message) from None
    ceiling = ceiling_from_fit(report, args.quant, args.bandwidth)

    def change(doc):
        doc["ceiling"] = ceiling

    answer = write_settings(gw, args.store, change)
    if args.json:
        print(json.dumps({"ceiling": ceiling, "settings": answer}, indent=2))
        return OK
    line = f"{answer.get('target')}: ceiling {ceiling['bytes_per_token'] / 1e9:.2f} GB per token"
    if ceiling.get("decode_tps"):
        line += f", decode up to {ceiling['decode_tps']:.0f} tok/s"
    print(line + f" (revision {answer.get('revision')})")
    return OK


def pct_words(value):
    return "-" if value is None else f"{value:.0f}%"


def num_words(value, unit=""):
    return "-" if value is None else f"{value:.0f}{unit}"


def cmd_doctor(gw, args):
    path = "/v1/optimize/report"
    if args.model:
        path += "?model=" + urllib.parse.quote(args.model, safe="")
    try:
        report = gw.get(path)
    except GatewayError as error:
        raise gateway_fail(error, "reading the speed report") from None
    if args.json:
        print(json.dumps(report, indent=2))
        return OK
    targets = report.get("targets") or []
    if not targets:
        print("no local targets to report on")
    rows = [("target", "decode*", "prompt", "ttft", "ceiling", "swaps/h", "cache", "accept",
             "empty", "waits", "wakes")]
    for t in targets:
        cache = (t.get("cache") or {}).get("token_hit_pct")
        accept = (t.get("speculative") or {}).get("acceptance_pct")
        wake = t.get("wake") or {}
        rows.append((t.get("target", "?"), num_words(t.get("decode_tps_p50")),
                     num_words(t.get("prompt_tps_p50")), num_words(t.get("ttft_ms_p50"), "ms"),
                     pct_words(t.get("pct_of_ceiling")), str(t.get("swaps_last_hour", "-")),
                     pct_words(cache), pct_words(accept), str(t.get("empty_after_thinking", "-")),
                     str(t.get("queue_waits", "-")), str(wake.get("count", "-"))))
    if targets:
        widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
        for row in rows:
            print("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)).rstrip())
    if targets:
        print("* decode: all requests, median (any concurrency, not a single-stream c1 figure)")
    for host in report.get("hosts") or []:
        print(f"host {host.get('host')}: {host.get('bandwidth_gbs')} GB/s ({host.get('source')})")
    findings = report.get("findings") or []
    if findings:
        print("findings")
        for f in findings:
            print(f"  {f.get('target')}: {f.get('evidence')}")
            print(f"    -> {f.get('action')}")
    elif targets:
        print("no findings")
    return OK


def model_path(model, suffix):
    return f"/v1/models/{urllib.parse.quote(model, safe='/')}/{suffix}"


def parse_assignment(text):
    """key=value; the value is JSON when it parses as JSON, else a string."""
    key, sep, raw = text.partition("=")
    key = key.strip()
    if not sep or not key:
        raise Fail(USAGE, f"`{text}` is not key=value")
    try:
        value = json.loads(raw)
    except ValueError:
        value = raw
    return key, value


def set_key(layer, path, value):
    """Set (or with None remove) one key path: `reasoning.enabled`, `extra.x.y`, `top_k`."""
    top, dot, field = path.partition(".")
    if not dot:
        if value is None:
            layer.pop(top, None)
        else:
            layer[top] = value
        return
    inner = layer.setdefault(top, {})
    if not isinstance(inner, dict):
        raise Fail(USAGE, f"`{top}` is not an object, so `{path}` cannot be set")
    if value is None:
        inner.pop(field, None)
        if not inner:
            layer.pop(top, None)
    else:
        inner[field] = value


def settings_view(gw, model, doing):
    try:
        return gw.get(model_path(model, "settings"))
    except GatewayError as error:
        raise gateway_fail(error, doing) from None


def one_target(view, model):
    targets = view.get("targets") or []
    # A public name can share its id with one of its targets (a route `mac/x`
    # whose first target is `mac/x`): that exact target is the one asked for.
    exact = [t for t in targets if t.get("target") == model]
    if len(targets) > 1 and len(exact) == 1:
        return exact[0]
    if len(targets) != 1:
        names = ", ".join(t.get("target", "?") for t in targets)
        raise Fail(USAGE, f"{model} reaches {len(targets)} targets ({names}); name one")
    return targets[0]


def value_words(value):
    if value is None:
        return "-"
    if isinstance(value, str):
        return value if len(value) <= 60 else value[:57] + "..."
    text = json.dumps(value)
    return text if len(text) <= 60 else text[:57] + "..."


def print_settings(view):
    for target in view.get("targets") or []:
        durable = "durable" if target.get("durable") else "memory only"
        line = f"{target.get('target')}  revision {target.get('revision', 0)} ({durable})"
        if target.get("route_profile"):
            line += f"  route profile {target['route_profile']}"
        print(line)
        effective = target.get("effective") or {}
        rows = [(key, value_words(row.get("value")), row.get("source", ""),
                 "locked" if row.get("locked") else "")
                for key, row in sorted(effective.items())]
        width = max([len(r[0]) for r in rows] + [3])
        vwidth = max([len(r[1]) for r in rows] + [5])
        for key, value, source, locked in rows:
            print(f"  {key.ljust(width)}  {value.ljust(vwidth)}  {source}{'  ' + locked if locked else ''}")
        profiles = target.get("profiles") or {}
        for name, layer in sorted(profiles.items()):
            keys = ", ".join(f"{k}={value_words(v)}" for k, v in sorted(layer.items()))
            print(f"  profile {name}: {keys or '(empty)'}")
        load = target.get("load")
        if isinstance(load, dict):
            print_load(load)


def print_load(load):
    engine = load.get("engine") or {}
    status = engine.get("status") or "unknown"
    print(f"  engine {engine.get('lane', '?')}: model {status}")
    for key, row in sorted((load.get("settings") or {}).items()):
        note = "  pending until reload" if row.get("pending") else ""
        print(f"  load {key} = {value_words(row.get('value'))}{note}")
    memory = load.get("memory")
    if isinstance(memory, dict):
        for name in ("min", "max", "effective"):
            part = memory.get(name)
            if isinstance(part, dict):
                print(f"  memory {name}: {part.get('bytes', 0) / 2**30:.1f} GiB at ctx {part.get('n_ctx', '?')}")


def cmd_settings(gw, args):
    if args.history:
        try:
            answer = gw.get(model_path(args.model, "settings/history"))
        except GatewayError as error:
            raise gateway_fail(error, "reading the settings history") from None
        if args.json:
            print(json.dumps(answer, indent=2))
            return OK
        print(f"{answer.get('target')}  revision {answer.get('revision', 0)}")
        for row in answer.get("history") or []:
            when = time.strftime("%Y-%m-%d %H:%M", time.localtime((row.get("at_ms") or 0) / 1000))
            changed = ", ".join(row.get("changed") or []) or "(no change)"
            print(f"  {row.get('revision'):>4}  {when}  {row.get('role', '')} {row.get('actor', '')}"
                  f"  {row.get('action', '')}: {changed}")
        return OK
    path = model_path(args.model, "settings")
    if args.profile:
        path += "?profile=" + urllib.parse.quote(args.profile)
    try:
        view = gw.get(path)
    except GatewayError as error:
        raise gateway_fail(error, "reading the settings") from None
    if args.json:
        print(json.dumps(view, indent=2))
    else:
        print_settings(view)
    return OK


def write_settings(gw, model, change, extra=None):
    """Read the document, change it, and write it back with its revision."""
    target = one_target(settings_view(gw, model, "reading the settings"), model)
    if "document" not in target:
        raise Fail(GATEWAY_ERROR, "this key may not change settings (an operator or admin key can)")
    doc = target.get("document") or {}
    change(doc)
    body = dict(doc)
    body.update(extra or {})
    try:
        return gw.call("PUT", model_path(target["target"], "settings"), body=body,
                       headers={"If-Match": str(target.get("revision", 0))}, timeout=120)
    except GatewayError as error:
        if error.status == 409 and error.reason == "model_busy":
            raise Fail(GATEWAY_ERROR, error.message) from None
        if error.status == 409:
            raise Fail(GATEWAY_ERROR, f"the settings changed meanwhile ({error.message}); "
                                      "run the command again") from None
        raise gateway_fail(error, "writing the settings") from None


def layer_of(doc, args):
    if args.profile:
        return doc.setdefault("profiles", {}).setdefault(args.profile, {})
    return doc.setdefault("defaults", {})


def clean_empty(doc):
    for name in ("defaults", "load", "engine_args") + DOC_KEYS:
        if name in doc and not doc[name]:
            doc.pop(name)
    if doc.get("reasoning") is False:
        doc.pop("reasoning")
    profiles = doc.get("profiles")
    if isinstance(profiles, dict) and not profiles:
        doc.pop("profiles")
    if "locked" in doc and not doc["locked"]:
        doc.pop("locked")


def cmd_set(gw, args):
    pairs = [parse_assignment(text) for text in args.pairs]
    load = getattr(args, "load", False)
    for key, value in pairs:
        top = key.partition(".")[0]
        if load and top not in LOAD_KEYS and top != "engine_args":
            raise Fail(USAGE, f"{key} is not a load setting ({', '.join(LOAD_KEYS)}, engine_args.KEY)")
        if not load and doc_level(key, value):
            if args.profile:
                raise Fail(USAGE, f"{key} belongs to the target, not to a profile")
            continue
        if not load and top not in REQUEST_KEYS:
            hint = " (a load setting: add --load)" if top in LOAD_KEYS else ""
            raise Fail(USAGE, f"{key} is not a request setting{hint}")

    def change(doc):
        for key, value in pairs:
            if load:
                if key.startswith("engine_args."):
                    set_key(doc.setdefault("engine_args", {}), key.partition(".")[2], value)
                else:
                    set_key(doc.setdefault("load", {}), key, value)
                    # A locked load key: `sb ai tune` never changes it.
                    if args.lock and f"load.{key}" not in doc.setdefault("locked", []):
                        doc["locked"].append(f"load.{key}")
                continue
            if doc_level(key, value):
                set_key(doc, key, value)
                continue
            set_key(layer_of(doc, args), key, value)
            if args.lock and key not in doc.setdefault("locked", []):
                doc["locked"].append(key)
        clean_empty(doc)

    extra = {}
    if load:
        extra["apply"] = "now" if args.apply == "now" else "next_load"
        if args.wake:
            extra["wake"] = True
    answer = write_settings(gw, args.model, change, extra)
    if args.json:
        print(json.dumps(answer, indent=2))
        return OK
    print(f"{answer.get('target')}: revision {answer.get('revision')}")
    apply = answer.get("apply")
    if isinstance(apply, dict) and apply.get("message"):
        print(apply["message"])
    return OK


def cmd_unset(gw, args):
    def change(doc):
        for key in args.keys:
            top = key.partition(".")[0]
            if doc_level(key, doc=doc) and not args.profile:
                set_key(doc, key, None)
            elif getattr(args, "load", False) or (top in LOAD_KEYS and top not in REQUEST_KEYS):
                set_key(doc.setdefault("load", {}), key, None)
                if f"load.{key}" in doc.get("locked", []):
                    doc["locked"].remove(f"load.{key}")
            elif key.startswith("engine_args."):
                set_key(doc.setdefault("engine_args", {}), key.partition(".")[2], None)
            else:
                set_key(layer_of(doc, args), key, None)
                if key in doc.get("locked", []):
                    doc["locked"].remove(key)
        if args.profile and not doc.get("profiles", {}).get(args.profile):
            doc.get("profiles", {}).pop(args.profile, None)
        clean_empty(doc)

    answer = write_settings(gw, args.model, change)
    print(f"{answer.get('target')}: revision {answer.get('revision')}")
    return OK


def cmd_revert(gw, args):
    # A revert is a write: it names the revision it edits, like set.
    target = one_target(settings_view(gw, args.model, "reading the settings"), args.model)
    try:
        answer = gw.call("POST", model_path(target["target"], "settings/revert"),
                         body={"revision": args.revision},
                         headers={"If-Match": str(target.get("revision", 0))}, timeout=120)
    except GatewayError as error:
        if error.status == 409:
            raise Fail(GATEWAY_ERROR, f"the settings changed meanwhile ({error.message}); "
                                      "run the command again") from None
        raise gateway_fail(error, "reverting the settings") from None
    print(f"{answer.get('target')}: revision {answer.get('revision')} "
          f"(the settings of revision {args.revision})")
    apply = answer.get("apply")
    if isinstance(apply, dict) and apply.get("message"):
        print(apply["message"])
    return OK


def cmd_load(gw, args):
    try:
        answer = gw.call("POST", model_path(args.model, "load"), body={"wake": bool(args.wake)},
                         timeout=args.timeout)
    except GatewayError as error:
        if error.status == 409:
            raise Fail(MACHINE_OFF, f"{error.message}") from None
        raise gateway_fail(error, f"loading {args.model}") from None
    if args.json:
        print(json.dumps(answer, indent=2))
        return OK
    line = f"{answer.get('target')}: {answer.get('status')}"
    memory = answer.get("memory") or {}
    for name in ("effective", "min"):
        part = memory.get(name) if isinstance(memory, dict) else None
        if isinstance(part, dict) and part.get("bytes"):
            line += f" ({part['bytes'] / 2**30:.1f} GiB on GPU at ctx {part.get('n_ctx', '?')})"
            break
    print(line)
    return OK


def cmd_unload(gw, args):
    try:
        answer = gw.post(model_path(args.model, "unload"), {})
    except GatewayError as error:
        raise gateway_fail(error, f"unloading {args.model}") from None
    print(f"{answer.get('target')}: {answer.get('message') or answer.get('status')}")
    return OK


def cmd_engines(gw, args):
    try:
        answer = gw.get("/v1/engines")
    except GatewayError as error:
        raise gateway_fail(error, "reading the engines") from None
    if args.json:
        print(json.dumps(answer, indent=2))
        return OK
    lanes = answer.get("data") or []
    if not lanes:
        print("no lane has an engine control")
        return OK
    for lane in lanes:
        seen = lane.get("seen_at_ms")
        when = (" (last seen " + time.strftime("%H:%M", time.localtime(seen / 1000)) + ")"
                if seen and lane.get("state") != "up" else "")
        print(f"{lane.get('lane')} ({lane.get('kind')}): {lane.get('state')}{when}")
        for row in lane.get("models") or []:
            notes = []
            memory = row.get("memory") or {}
            part = memory.get("effective") or memory.get("min") if isinstance(memory, dict) else None
            if isinstance(part, dict) and part.get("bytes"):
                notes.append(f"{part['bytes'] / 2**30:.1f} GiB at ctx {part.get('n_ctx', '?')}")
            if row.get("pending"):
                notes.append("settings pending until reload")
            if row.get("in_flight"):
                notes.append(f"{row['in_flight']} in flight")
            if row.get("failed"):
                notes.append("last load failed")
            extra = f"  ({'; '.join(notes)})" if notes else ""
            print(f"  {row.get('id')}  {row.get('status')}{extra}")
        for variant in lane.get("variants") or []:
            print(f"  variant {variant.get('name')} of {variant.get('target')}: "
                  f"{json.dumps(variant.get('load'))}")
        apply = lane.get("last_apply")
        if isinstance(apply, dict) and not apply.get("ok", True):
            print(f"  last apply failed: {apply.get('error')}")
    return OK


def cmd_chat(gw, args):
    settings = {}
    for text in args.set or []:
        key, value = parse_assignment(text)
        set_key(settings, key, value)
    body = {"model": args.model, "messages": [{"role": "user", "content": args.prompt}],
            "stream": True, "stream_options": {"include_usage": True}}
    load = {}
    for text in args.load or []:
        key, value = parse_assignment(text)
        load[key] = value
    if settings or args.profile or load:
        body["switchback"] = {}
        if settings:
            body["switchback"]["settings"] = settings
        if args.profile:
            body["switchback"]["profile"] = args.profile
        if load:
            body["switchback"]["load"] = load
    answer_headers, text, reasoning = {}, [], []
    started = time.monotonic()
    first, usage, timings = None, None, None
    try:
        for when, chunk in gw.stream("/v1/chat/completions", body, timeout=args.timeout,
                                     answer_headers=answer_headers):
            if isinstance(chunk.get("timings"), dict):
                timings = chunk["timings"]
            if isinstance(chunk.get("usage"), dict) and chunk["usage"]:
                usage = chunk["usage"]
            for choice in chunk.get("choices") or []:
                delta = choice.get("delta") or choice.get("message") or {}
                if delta.get("content"):
                    text.append(delta["content"])
                if delta.get("reasoning_content") or delta.get("reasoning"):
                    reasoning.append(delta.get("reasoning_content") or delta.get("reasoning"))
            if first is None and chunk_has_text(chunk):
                first = when
    except GatewayError as error:
        raise gateway_fail(error, f"asking {args.model}") from None
    ended = time.monotonic()
    usage = usage or {}
    completion = int(usage.get("completion_tokens") or 0)
    decoded = ended - first if first else 0
    speed = completion / decoded if decoded > 0 else 0.0
    if timings and number_of(timings.get("predicted_per_second")) is not None:
        speed = number_of(timings.get("predicted_per_second"))
    result = {"model": args.model, "answer": "".join(text),
              "reasoning": "".join(reasoning) or None,
              "prompt_tokens": int(usage.get("prompt_tokens") or 0),
              "completion_tokens": completion,
              "ttft_ms": round((first - started) * 1000) if first else None,
              "decode_tok_s": round(speed, 1), "total_s": round(ended - started, 2),
              "route": answer_headers.get("x-switchback-route"),
              "settings": answer_headers.get("x-switchback-settings")}
    if args.json:
        print(json.dumps(result, indent=2))
        return OK
    print(result["answer"])
    ttft = f"{result['ttft_ms']} ms" if result["ttft_ms"] is not None else "-"
    log(f"tokens: {result['prompt_tokens']} in, {completion} out; first token {ttft}; "
        f"{result['decode_tok_s']} tok/s; {result['total_s']} s")
    if result["route"]:
        log(f"route: {result['route']}")
    if result["settings"]:
        log(f"settings: {result['settings']}")
    return OK


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
    sub.choices["models"].add_argument("--all", action="store_true")
    bench = sub.add_parser("bench", add_help=False, allow_abbrev=False)
    bench.add_argument("model", nargs="*")
    bench.add_argument("--workload", choices=("chat", "read", "all"), default="all")
    bench.add_argument("--runs", type=int, default=BENCH_RUNS)
    bench.add_argument("--wake", action="store_true")
    bench.add_argument("--keep-cache", action="store_true")
    bench.add_argument("--json", action="store_true")
    bench.add_argument("-o", "--output")
    hold = sub.add_parser("hold", add_help=False, allow_abbrev=False)
    hold.add_argument("duration")
    hold.add_argument("reason", nargs="?", default="")
    hold.add_argument("--wake", action="store_true")
    hold.add_argument("--host")
    hold.add_argument("--load")
    hold.add_argument("--json", action="store_true")
    release = sub.add_parser("release", add_help=False, allow_abbrev=False)
    release.add_argument("id", nargs="?")
    release.add_argument("--all", action="store_true")
    release.add_argument("--host")
    on = sub.add_parser("on", add_help=False, allow_abbrev=False)
    on.add_argument("duration", nargs="?", default="")
    on.add_argument("reason", nargs="?", default="")
    on.add_argument("--host")
    on.add_argument("--json", action="store_true")
    off = sub.add_parser("off", add_help=False, allow_abbrev=False)
    off.add_argument("--release-holds", action="store_true")
    off.add_argument("--host")
    segment = sub.add_parser("segment", add_help=False, allow_abbrev=False)
    segment.add_argument("image")
    segment.add_argument("text", nargs="?", default="")
    segment.add_argument("--model", default="")
    segment.add_argument("-o", "--output")
    segment.add_argument("--timeout", type=float, default=900)
    transcribe = sub.add_parser("transcribe", add_help=False, allow_abbrev=False)
    transcribe.add_argument("file")
    transcribe.add_argument("--model", default="")
    transcribe.add_argument("--language", default="")
    transcribe.add_argument("--speakers", action="store_true")
    transcribe.add_argument("--format", choices=("text", "srt", "vtt", "json"))
    transcribe.add_argument("-o", "--output")
    transcribe.add_argument("--timeout", type=float, default=900)
    speak = sub.add_parser("speak", add_help=False, allow_abbrev=False)
    speak.add_argument("text")
    speak.add_argument("--model", default="")
    speak.add_argument("--voice", default="")
    speak.add_argument("-o", "--output", required=True)
    speak.add_argument("--timeout", type=float, default=900)
    fit = sub.add_parser("fit", add_help=False, allow_abbrev=False)
    ai_fit.add_arguments(fit)
    fit.add_argument("--store")
    fit.add_argument("--quant")
    doctor = sub.add_parser("doctor", add_help=False, allow_abbrev=False)
    doctor.add_argument("model", nargs="?", default="")
    doctor.add_argument("--json", action="store_true")
    ai_tune.add_arguments(sub.add_parser("tune", add_help=False, allow_abbrev=False))
    settings = sub.add_parser("settings", add_help=False, allow_abbrev=False)
    settings.add_argument("model")
    settings.add_argument("--profile")
    settings.add_argument("--history", action="store_true")
    settings.add_argument("--json", action="store_true")
    set_cmd = sub.add_parser("set", add_help=False, allow_abbrev=False)
    set_cmd.add_argument("model")
    set_cmd.add_argument("pairs", nargs="+")
    set_cmd.add_argument("--profile")
    set_cmd.add_argument("--lock", action="store_true")
    set_cmd.add_argument("--load", action="store_true")
    set_cmd.add_argument("--apply", choices=("next-load", "now"), default="next-load")
    set_cmd.add_argument("--wake", action="store_true")
    set_cmd.add_argument("--json", action="store_true")
    unset = sub.add_parser("unset", add_help=False, allow_abbrev=False)
    unset.add_argument("model")
    unset.add_argument("keys", nargs="+")
    unset.add_argument("--profile")
    unset.add_argument("--load", action="store_true")
    revert = sub.add_parser("revert", add_help=False, allow_abbrev=False)
    revert.add_argument("model")
    revert.add_argument("revision", type=int)
    load = sub.add_parser("load", add_help=False, allow_abbrev=False)
    load.add_argument("model")
    load.add_argument("--wake", action="store_true")
    load.add_argument("--json", action="store_true")
    load.add_argument("--timeout", type=float, default=900)
    unload_cmd = sub.add_parser("unload", add_help=False, allow_abbrev=False)
    unload_cmd.add_argument("model")
    sub.add_parser("engines", add_help=False, allow_abbrev=False).add_argument("--json", action="store_true")
    chat = sub.add_parser("chat", add_help=False, allow_abbrev=False)
    chat.add_argument("model")
    chat.add_argument("prompt")
    chat.add_argument("--profile")
    chat.add_argument("--set", action="append")
    chat.add_argument("--load", action="append")
    chat.add_argument("--json", action="store_true")
    chat.add_argument("--timeout", type=float, default=900)
    return top


def main(argv=None, environ=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    environ = os.environ if environ is None else environ
    if not argv or argv[0] == "help" or "-h" in argv or "--help" in argv:
        print(HELP, end="")
        return OK
    args = parser().parse_args(argv)
    if args.command == "fit" and not args.store:
        # fit reads the model index, not the gateway: it needs no key.
        return ai_fit.run(args)
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
        if args.command == "bench":
            return cmd_bench(gw, args)
        if args.command == "hold":
            return cmd_hold(gw, args)
        if args.command == "release":
            return cmd_release(gw, args)
        if args.command == "on":
            return cmd_on(gw, args)
        if args.command == "off":
            return cmd_off(gw, args)
        if args.command == "segment":
            return cmd_segment(gw, config, args)
        if args.command == "transcribe":
            return cmd_transcribe(gw, args)
        if args.command == "speak":
            return cmd_speak(gw, config, args)
        if args.command == "settings":
            return cmd_settings(gw, args)
        if args.command == "set":
            return cmd_set(gw, args)
        if args.command == "unset":
            return cmd_unset(gw, args)
        if args.command == "revert":
            return cmd_revert(gw, args)
        if args.command == "chat":
            return cmd_chat(gw, args)
        if args.command == "load":
            return cmd_load(gw, args)
        if args.command == "unload":
            return cmd_unload(gw, args)
        if args.command == "engines":
            return cmd_engines(gw, args)
        if args.command == "fit":
            return cmd_fit_store(gw, args)
        if args.command == "doctor":
            return cmd_doctor(gw, args)
        if args.command == "tune":
            return ai_tune.run(gw, args, (Fail, GatewayError),
                               poll_secs=float(environ.get("SB_AI_POLL_SECS") or 2))
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
