#!/usr/bin/env python3
"""sb ai check: does a local model's memory and context setting hold up in
real use?

Three checks, each sent through the gateway like any request:

  long     one request at about 90% of the context the model is set to,
           with three fresh codes at 10%, 50% and 90% depth. It passes only
           on HTTP 200 with all three codes in the answer.
  pool     one long request plus fillers sent at the same time, together
           filling the engine's context pool without going over it. It fails
           when any request fails, or when the model's process crashed,
           restarted or was unloaded during the run.
  picture  (only for a model with a projector) a picture with a fresh number
           and shape, sent N times (12 by default); it reports how many came
           back right.

Every request carries fresh random text (no two prompts share a beginning, so
the engine reads every one in full) and asks the gateway not to use a cached
result. An answer cut at max_tokens, or empty, is reported on its own: it
says nothing about memory or context, so it is never counted as wrong.

Every result goes to the gateway's quality ledger (suites check-long,
check-pool, check-picture), tied to the target, its settings revision, the
engine build and the model file.

`set --check`, `load` and `sb ai tune` use the same checks as a guard: a
change to a memory or context setting must pass them, or it is put back.

Standard library only.
"""

import base64
import json
import random
import re
import struct
import sys
import threading
import time
import urllib.parse
import uuid
import zlib
from concurrent.futures import ThreadPoolExecutor

OK, FAIL, USAGE, MACHINE_OFF = 0, 1, 2, 3
INTERRUPTED = 130

CHECKS = ("long", "pool", "picture")
SUITE_PREFIX = "check-"
SUITE_VERSION = "1"
LOCK_HEADER = "x-switchback-tune-lock"
UNKNOWN = "unknown"

# Load settings (and engine arguments) that change how much memory a model
# takes or how much context one request gets. A change to any of them is
# guarded by the checks.
GUARDED_LOAD = ("ctx_size", "parallel", "cache_type_k", "cache_type_v", "batch_size",
                "ubatch_size", "n_gpu_layers", "fit", "fit_ctx", "mmproj")
GUARDED_ARGS = ("ctx-size", "parallel", "cache-type-k", "cache-type-v", "batch-size",
                "ubatch-size", "kv-unified", "no-kv-unified", "n-cpu-moe", "cpu-moe",
                "override-tensor", "fit", "fit-ctx", "fit-target", "n-gpu-layers",
                "gpu-layers", "kv-offload", "no-kv-offload", "swa-full", "mmproj-offload",
                "no-mmproj-offload")

LONG_SHARE = 0.90       # the long request: this share of the context it tests
POOL_SHARE = 0.97       # all pool requests together (prompts and answers): this share of the pool
DEPTHS = (0.10, 0.50, 0.90)
PLACES = ("Larkspur", "Wrenfield", "Ostmark")
CODE_WORDS = ("QUILL", "EMBER", "FJORD", "LUMEN", "CEDAR", "ONYX", "BASALT", "HERON", "TALON",
              "SORREL", "MAPLE", "COBALT", "SPRUCE", "AMBER", "GARNET", "OSPREY")
LONG_MAX_TOKENS = 4096   # room for a thinking model; a cut answer is reported as capped
FILL_MAX_TOKENS = 1024
PICTURE_MAX_TOKENS = 2048
PICTURE_RUNS = 12
PICTURE_FLOOR = 0.9      # with nothing to compare against: right at least this share of answered runs
PROBE_TOKENS = 1500
FIRST_GUESS_CHARS = 5.0  # characters per token before the probe measured it
MAX_READ_TPS = 200000.0  # faster than this a long prompt was not read: it came from a cache
MIN_FILL_TOKENS = 1000   # a filler shorter than this is left out
REQUEST_TIMEOUT_S = 1200
PAUSED_WAIT_S = 300
WATCH_EVERY_S = 2.0
LOAD_TIMEOUT_S = 900
WAKE_TIMEOUT_S = 600
MAX_LOCK_S = 14400

READY_STATES = ("healthy", "idle_countdown")
ON_STATES = ("healthy", "draining", "idle_countdown", "degraded")
RUNNING = ("loaded", "sleeping", "loading")

WORDS = """
able about above across after again against along already also always among
answer apple april area around autumn back badge baker basin battle beach beacon
before behind below bench beside better between beyond bird black blanket blue
board boat bottle bread bridge bright broad brother brown bucket build burden
butter cabin cable camera canal candle canyon carbon careful carpet castle cattle
cedar cellar center chain chair chalk channel chapter charcoal cheese cherry
circle city clay clean clever cliff clock cloud coast coffee collar colony comet
common copper corner cotton county courage cradle crane creek crowd crystal
current curtain daily dancer danger dawn decade deep delta desert detail
diamond dinner direct distant doctor dollar double dragon drawer dream driver
early earth eastern easy echo edge eight elbow elder empty engine evening
exact fabric factory fair falcon family famous farmer father feather fellow
fence field final finger fire flame flat flower forest forge fortune fountain
frame fresh friend frozen fruit garden garlic gather gentle giant glacier glass
golden grain grand granite grass gravel green ground guard guitar habit hammer
harbor harvest hazel heavy helmet hidden highway hollow honey horizon horse
hunter island ivory jacket jungle kettle kitchen ladder lake lantern large later
leather lemon letter level light limestone linen little lizard local lodge lumber
lunar magnet mallet manor marble market meadow medal melody metal middle mill
mineral minute mirror modest moment morning mother motor mountain narrow nature
needle nephew never noble north number ocean office olive orange orchard outer
oxygen paddle palace paper parcel pattern pebble pencil people pepper perfect
pillow pilot planet plaster pocket polar pond popular powder prairie present
quarry quiet rabbit radio rapid raven recent ribbon ridge river rocket rough
saddle salmon sample sandy season second secret shadow shelter shore silent
silver simple sister slate smooth soft solid spring square stable steady stone
storm stream strong summer sunset swift table tailor teacher tender thunder
timber today tower travel tribe trophy tunnel twelve valley velvet village
violet voyage wagon walnut water weather western wheat whisper willow window
winter wooden worker yellow yonder young zephyr
""".split()

# A 5x7 pixel font for the digits the picture check draws.
DIGITS = {
    "0": ("01110", "10001", "10011", "10101", "11001", "10001", "01110"),
    "1": ("00100", "01100", "00100", "00100", "00100", "00100", "01110"),
    "2": ("01110", "10001", "00001", "00010", "00100", "01000", "11111"),
    "3": ("11110", "00001", "00001", "01110", "00001", "00001", "11110"),
    "4": ("00010", "00110", "01010", "10010", "11111", "00010", "00010"),
    "5": ("11111", "10000", "11110", "00001", "00001", "10001", "01110"),
    "6": ("00110", "01000", "10000", "11110", "10001", "10001", "01110"),
    "7": ("11111", "00001", "00010", "00100", "01000", "01000", "01000"),
    "8": ("01110", "10001", "10001", "01110", "10001", "10001", "01110"),
    "9": ("01110", "10001", "10001", "01111", "00001", "00010", "01100"),
}
SHAPES = ("circle", "square", "triangle")
COLOURS = {"red": (220, 30, 30), "blue": (30, 70, 220), "green": (30, 160, 60)}


class CheckError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


# ------------------------------------------------------------------ the truth view

def arg_value(args, names):
    """The last value of a flag in an argument list (`--name value` or
    `--name=value`); a flag with no value after it reads "on"."""
    out = None
    for i, arg in enumerate(args):
        for name in names:
            if arg == name:
                nxt = args[i + 1] if i + 1 < len(args) else None
                if nxt is not None and (not nxt.startswith("-") or _is_number(nxt)):
                    out = nxt
                else:
                    out = "on"
            elif arg.startswith(name + "="):
                out = arg[len(name) + 1:]
    return out


def _is_number(text):
    try:
        float(text)
        return True
    except ValueError:
        return False


def _int(text):
    try:
        return int(float(text))
    except (TypeError, ValueError):
        return None


def truth_from_args(args):
    """What the engine runs a model with, read from its argument list: the
    set context, the slots, whether the slots share one pool (kv-unified),
    the context one request can use, the cache types, and whether weights
    may sit on the CPU. The same rules as the gateway's `effective` view."""
    args = [str(a) for a in args or []]
    if not args:
        return None
    ctx = _int(arg_value(args, ("--ctx-size", "-c")))
    slots_text = arg_value(args, ("--parallel", "-np"))
    slots = _int(slots_text)
    unified_on = any(a in ("--kv-unified", "-kvu") for a in args)
    unified_off = any(a in ("--no-kv-unified", "-no-kvu") for a in args)
    if unified_off:
        unified = False
    elif unified_on:
        unified = True
    elif slots is None:
        unified = None  # the engine picks slots and sharing itself
    else:
        unified = False
    per_request = None
    if ctx and slots:
        per_request = ctx if unified else ctx // max(1, slots)
    elif ctx and unified:
        per_request = ctx
    ngl = arg_value(args, ("--n-gpu-layers", "--gpu-layers", "-ngl"))
    fit = arg_value(args, ("--fit", "-fit"))
    n_cpu_moe = _int(arg_value(args, ("--n-cpu-moe", "-ncmoe")))
    cpu_moe = any(a in ("--cpu-moe", "-cmoe") for a in args)
    overrides = [args[i + 1] for i, a in enumerate(args[:-1]) if a in ("--override-tensor", "-ot")]
    overrides += [a.split("=", 1)[1] for a in args if a.startswith("--override-tensor=")]
    cpu_overrides = [o for o in overrides if "=cpu" in o.lower()]
    if n_cpu_moe or cpu_moe or cpu_overrides:
        cpu_weights = "yes"
    elif ngl is not None and _int(ngl) is not None and _int(ngl) < 999:
        cpu_weights = "maybe"  # some layers may stay on the CPU, depending on the model's layer count
    elif (fit or "").lower() in ("on", "true", "1"):
        cpu_weights = "maybe"  # the engine fits the model to free memory and may keep layers on the CPU
    elif ngl is not None or (fit or "").lower() in ("off", "false", "0"):
        cpu_weights = "no"
    else:
        cpu_weights = "unknown"
    return {
        "source": "args",
        "ctx_size": ctx,
        "slots": slots,
        "kv_unified": unified,
        "per_request_ctx": per_request,
        "pool_ctx": ctx,
        "cache_type_k": arg_value(args, ("--cache-type-k", "-ctk")) or "f16",
        "cache_type_v": arg_value(args, ("--cache-type-v", "-ctv")) or "f16",
        "batch_size": _int(arg_value(args, ("--batch-size", "-b"))),
        "ubatch_size": _int(arg_value(args, ("--ubatch-size", "-ub"))),
        "n_gpu_layers": ngl,
        "fit": fit,
        "n_cpu_moe": n_cpu_moe or 0,
        "cpu_moe": cpu_moe,
        "cpu_tensor_overrides": len(cpu_overrides),
        "cpu_weights": cpu_weights,
        "projector": any(a in ("--mmproj", "-mm") or a.startswith("--mmproj=") for a in args),
        "port": _int(arg_value(args, ("--port",))),
    }


def truth_of(row):
    """The truth view of an engine listing row: the gateway's own
    `effective` when it names one, else read from the row's arguments."""
    effective = row.get("effective") if isinstance(row, dict) else None
    if isinstance(effective, dict) and effective.get("ctx_size") is not None:
        out = dict(effective)
        if out.get("port") is None:
            port = (truth_from_args(row.get("args")) or {}).get("port")
            out["port"] = port
        return out
    return truth_from_args((row or {}).get("args"))


def split_finding(truth):
    """Why one request cannot use the whole set context, or None."""
    if not truth:
        return None
    ctx, per, slots = truth.get("ctx_size"), truth.get("per_request_ctx"), truth.get("slots")
    if ctx and per and per < ctx:
        return (f"the set context {ctx:,} is split over {slots} slots (kv-unified off): one request "
                f"can use {per:,}")
    return None


def truth_words(truth):
    if not truth:
        return "the engine lists no arguments for it"
    per = truth.get("per_request_ctx")
    unified = {True: "shared", False: "split", None: "engine's choice"}[truth.get("kv_unified")]
    parts = [f"context {fmt(truth.get('ctx_size'))}",
             f"{truth.get('slots') or '?'} slot(s), {unified}",
             f"one request {fmt(per)}",
             f"KV {truth.get('cache_type_k')}/{truth.get('cache_type_v')}",
             f"weights on CPU: {truth.get('cpu_weights')}"]
    if truth.get("projector"):
        parts.append("projector")
    if truth.get("source") == "props":
        parts.append("read from the engine")
    return ", ".join(parts)


def fmt(number):
    return f"{number:,}" if isinstance(number, int) else "?"


def guarded_changes(before, after):
    """The guarded load settings and engine arguments that differ between
    two settings documents, as `load.KEY` / `engine_args.NAME`."""
    before, after = before or {}, after or {}
    out = []
    for key in GUARDED_LOAD:
        if (before.get("load") or {}).get(key) != (after.get("load") or {}).get(key):
            out.append(f"load.{key}")
    for name in GUARDED_ARGS:
        if (before.get("engine_args") or {}).get(name) != (after.get("engine_args") or {}).get(name):
            out.append(f"engine_args.{name}")
    return out


# ------------------------------------------------------------------ fresh text and pictures

def answer_room(ctx, most):
    """Tokens kept free for the answer of a request at a context of `ctx`:
    `most`, but never more than an eighth of the context."""
    return min(most, max(256, int(ctx) // 8))


def fresh_rng():
    return random.Random(uuid.uuid4().int)


def fresh_code(rng):
    return f"{rng.randrange(1000, 10000)}-{rng.choice(CODE_WORDS)}{rng.randrange(16 * 16):02X}"


def fresh_text(rng, chars):
    """At least `chars` characters of random sentences. It starts with a
    random run line, so no two prompts share a beginning."""
    out = [f"Record {uuid.uuid4().hex[:16]}.\n"]
    size = len(out[0])
    while size < chars:
        sentences = []
        for _ in range(rng.randrange(3, 8)):
            words = [rng.choice(WORDS) for _ in range(rng.randrange(6, 17))]
            words[0] = words[0].capitalize()
            sentences.append(" ".join(words) + rng.choice(".....;!?"))
        paragraph = " ".join(sentences) + "\n\n"
        out.append(paragraph)
        size += len(paragraph)
    return "".join(out)


def long_prompt(rng, tokens, chars_per_token):
    """A prompt of about `tokens` tokens with three fresh codes at 10%, 50%
    and 90% depth. Returns (prompt, codes)."""
    question = ("\n\nQuestion: the text above contains three notes with vault access codes, for "
                "the Larkspur, Wrenfield and Ostmark archives. List all three codes, one per "
                "line, as 'Archive: code'. Reply with the three lines only.")
    chars = max(200, int(tokens * chars_per_token) - len(question))
    body = fresh_text(rng, chars)[:chars]
    codes = [fresh_code(rng) for _ in PLACES]
    parts, prev = [], 0
    for depth, place, code in zip(DEPTHS, PLACES, codes):
        cut = body.rfind("\n", 0, int(len(body) * depth)) + 1
        cut = max(cut, prev)
        parts.append(body[prev:cut])
        parts.append(f"\nNOTE FOR THE READER: the vault access code for the {place} archive "
                     f"is {code}.\n")
        prev = cut
    parts.append(body[prev:])
    return "".join(parts) + question, codes


def codes_found(text, codes):
    flat = re.sub(r"\s+", "", (text or "").upper())
    return [code for code in codes if code in flat]


def png(width, height, pixels):
    """A PNG file (8-bit RGB) of `pixels`, a bytearray of width*height*3."""
    rows = bytearray()
    stride = width * 3
    for y in range(height):
        rows.append(0)
        rows.extend(pixels[y * stride:(y + 1) * stride])

    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(rows), 9)) + chunk(b"IEND", b""))


def picture(rng):
    """A fresh picture: a random three-digit number in black and a random
    shape in a random colour next to it. Returns (png bytes, number, shape,
    colour)."""
    width, height = 640, 360
    pixels = bytearray([255]) * (width * height * 3)

    def put(x, y, rgb):
        if 0 <= x < width and 0 <= y < height:
            i = (y * width + x) * 3
            pixels[i:i + 3] = bytes(rgb)

    number = str(rng.randrange(100, 1000))
    scale, left, top = 18, 40 + rng.randrange(0, 20), 50 + rng.randrange(0, 60)
    for n, digit in enumerate(number):
        x0 = left + n * 6 * scale
        for row, bits in enumerate(DIGITS[digit]):
            for col, bit in enumerate(bits):
                if bit == "1":
                    for dy in range(scale):
                        for dx in range(scale):
                            put(x0 + col * scale + dx, top + row * scale + dy, (0, 0, 0))
    shape = rng.choice(SHAPES)
    colour = rng.choice(sorted(COLOURS))
    rgb = COLOURS[colour]
    cx, cy, r = 520, 180, 80
    for y in range(cy - r, cy + r + 1):
        for x in range(cx - r, cx + r + 1):
            dx, dy = x - cx, y - cy
            inside = (shape == "circle" and dx * dx + dy * dy <= r * r) or shape == "square" or (
                shape == "triangle" and dy >= -r and abs(dx) <= (dy + r) / 2)
            if inside:
                put(x, y, rgb)
    return png(width, height, pixels), number, shape, colour


# ------------------------------------------------------------------ one request

def classify(result, codes=None):
    """One answer's result word: ok, context_exceeded, request_failed,
    not_read (it came back faster than the prompt can be read), length_capped,
    empty_reply or codes_missing."""
    status = result.get("status")
    if status != 200:
        message = (result.get("error") or "").lower()
        if status == 400 and "context" in message:
            return "context_exceeded"
        return "request_failed"
    if result.get("not_read"):
        return "not_read"
    if result.get("finish") == "length":
        return "length_capped"
    if not (result.get("text") or "").strip():
        return "empty_reply"
    if codes is not None and len(codes_found(result.get("text"), codes)) < len(codes):
        return "codes_missing"
    return "ok"


# ------------------------------------------------------------------ the checker

class Checker:
    def __init__(self, gw, args, fail_types, out=sys.stdout, err=sys.stderr, poll_secs=None):
        self.gw = gw
        self.args = args
        self.fail_types = fail_types
        self.out = out
        self.err = err
        self.poll = 2.0 if poll_secs is None else max(0.01, float(poll_secs))
        self.scale = 1.0 if poll_secs is None else max(0.001, float(poll_secs) / 2.0)
        self.lock_id = None
        self.hold = None
        self.chars_per_token = FIRST_GUESS_CHARS
        self.no_cache = None  # whether the gateway takes `switchback.no_cache`
        self.ident = None
        self.truth = None
        self.row = None

    # ------------------------------------------------------------ talking

    def log(self, message):
        if not getattr(self.args, "json", False):
            print(message, file=self.out, flush=True)

    def say(self, message):
        print(f"sb ai check: {message}", file=self.err, flush=True)

    def call(self, method, path, body=None, timeout=60, headers=None, doing="talking to the gateway"):
        try:
            return self.gw.call(method, path, body=body, timeout=timeout, headers=headers or {})
        except self.fail_types as error:
            code = getattr(error, "code", None)
            off = code == MACHINE_OFF and not hasattr(error, "status")
            raise CheckError(MACHINE_OFF if off else FAIL,
                             f"{doing}: {getattr(error, 'message', error)}") from None

    def model_path(self, target, suffix):
        return f"/v1/models/{urllib.parse.quote(target, safe='/')}/{suffix}"

    # ------------------------------------------------------------ identity

    def resolve(self, name):
        view = self.call("GET", self.model_path(name, "settings"), doing=f"reading the settings of {name}")
        targets = view.get("targets") or []
        exact = [t for t in targets if t.get("target") == name]
        if len(targets) > 1 and len(exact) == 1:
            targets = exact
        if len(targets) != 1:
            names = ", ".join(t.get("target", "?") for t in targets)
            raise CheckError(USAGE, f"{name} reaches {len(targets)} targets ({names}): checks run "
                                    "on one target; name it as provider/model")
        target = targets[0]
        if "document" not in target:
            raise CheckError(FAIL, "checks need an operator or admin key")
        full = target["target"]
        provider, _, model = full.partition("/")
        engine = (target.get("load") or {}).get("engine") or {}
        return {"name": name, "target": full, "model": model, "lane": engine.get("lane") or provider,
                "revision": int(target.get("revision") or 0),
                "settings_hash": str(target.get("settings_hash") or UNKNOWN),
                "document": target.get("document") or {}}

    def engine_row(self, ident):
        """The model's engine listing row and its lane's build. A sleeping
        machine is never called: the gateway answers from its last listing."""
        engines = self.call("GET", "/v1/engines", timeout=30, doing="reading the engines")
        for lane in engines.get("data") or []:
            if lane.get("lane") != ident["lane"]:
                continue
            for row in lane.get("models") or []:
                if row.get("id") == ident["model"]:
                    return row, str(lane.get("build") or UNKNOWN)
            return None, str(lane.get("build") or UNKNOWN)
        return None, UNKNOWN

    def engine_props(self, ident):
        """The engine's own numbers for a loaded model (`/props`): context per
        slot and slot count. Only a gateway that has the endpoint answers."""
        try:
            return self.gw.call("GET", self.model_path(ident["target"], "engine-props"), timeout=15,
                                headers={})
        except self.fail_types:
            return None

    def read_truth(self, ident):
        row, build = self.engine_row(ident)
        self.row = row or {}
        truth = truth_of(self.row)
        props = self.engine_props(ident)
        if truth is not None and isinstance(props, dict):
            per_slot, slots = props.get("n_ctx_per_slot"), props.get("slots")
            if isinstance(per_slot, int) and per_slot > 0:
                truth = dict(truth, per_slot_ctx_engine=per_slot, source="props")
                if isinstance(slots, int) and slots > 0:
                    truth["slots"] = slots
                if not truth.get("kv_unified"):
                    truth["per_request_ctx"] = per_slot
            if props.get("vision") is True:
                truth["projector"] = True
        file = self.row.get("file")
        if not file and self.row.get("args"):
            file = "args-" + hex(zlib.crc32(json.dumps(self.row["args"]).encode()))[2:]
        ident.update(engine_build=build, model_file=str(file or UNKNOWN))
        self.truth = truth
        return truth

    # ------------------------------------------------------------ machine, hold and lock

    def lanes(self):
        return self.call("GET", "/v1/workloads/capacity", timeout=15, doing="reading the lanes").get("data") or []

    def preflight(self, ident):
        lanes = self.lanes()
        lane = next((l for l in lanes if l.get("name") == ident["lane"]), None)
        if lane is None:
            raise CheckError(FAIL, f"{ident['target']}: the gateway reports no lane {ident['lane']} "
                                   "(checks run on a local lane with engine control)")
        if lane.get("experiments") is False:
            raise CheckError(FAIL, f"lane `{ident['lane']}` does not allow experiments "
                                   "(local_executors[].experiments: false); nothing was held or woken")
        host = lane.get("host") or lane.get("name")
        states = {l.get("state") for l in lanes if (l.get("host") or l.get("name")) == host}
        power = "on" if states & set(ON_STATES) else (
            "waking" if "waking" in states else "powering off" if "powering_off" in states else "off")
        ident.update(host=host, asleep=power != "on")
        if power in ("off", "powering off") and not getattr(self.args, "wake", False):
            raise CheckError(MACHINE_OFF, f"{host} is {power}; rerun with --wake to let the gateway "
                                          "wake it, or the checks cannot run")

    def take_hold_and_lock(self, ident, why):
        ttl = MAX_LOCK_S // 4
        if ident["asleep"]:
            self.take_lock(ident, why, ttl, None)
            self.take_hold(ident, why, ttl, wake=bool(getattr(self.args, "wake", False)))
            self.wait_awake(ident)
        else:
            self.take_hold(ident, why, ttl, wake=False)
            self.take_lock(ident, why, ttl, self.hold[1])

    def take_hold(self, ident, why, ttl, wake):
        hold = self.call("POST", f"/v1/hosts/{urllib.parse.quote(ident['host'])}/holds",
                         body={"ttl_secs": ttl, "reason": why, "wake": wake, "until_quiet_window": True},
                         doing="holding the machine")
        self.hold = (ident["host"], hold.get("id"))

    def take_lock(self, ident, why, ttl, hold_id):
        body = {"target": ident["target"], "ttl_secs": ttl, "reason": why}
        if hold_id:
            body["hold_id"] = hold_id
        try:
            lock = self.call("POST", "/v1/optimize/locks", body=body, doing="taking the lane's tuning lock")
        except CheckError as error:
            raise CheckError(FAIL, error.message.replace("taking the lane's tuning lock: ", "")) from None
        self.lock_id = lock.get("id")

    def wait_awake(self, ident):
        deadline = time.monotonic() + WAKE_TIMEOUT_S * self.scale
        while time.monotonic() < deadline:
            lane = next((l for l in self.lanes() if l.get("name") == ident["lane"]), None)
            if lane and lane.get("state") in READY_STATES:
                return
            time.sleep(self.poll)
        raise CheckError(FAIL, f"the machine of {ident['target']} did not come up within {WAKE_TIMEOUT_S} s")

    def renew(self):
        if not self.lock_id:
            return
        try:
            beat = self.gw.call("PATCH", f"/v1/optimize/locks/{self.lock_id}",
                                body={"ttl_secs": MAX_LOCK_S // 4}, timeout=30, headers={})
        except self.fail_types as error:
            if getattr(error, "status", None) == 404:
                self.lock_id = None
                raise CheckError(FAIL, "the tuning lock is gone (it ran out or the gateway restarted)") from None
            return
        if beat.get("quiet_window"):
            raise CheckError(OK, f"the quiet window {beat['quiet_window']} began; checks stopped")

    def release(self):
        if self.lock_id:
            try:
                self.gw.call("DELETE", f"/v1/optimize/locks/{self.lock_id}", timeout=30, headers={})
            except self.fail_types as error:
                self.say(f"releasing the lock: {getattr(error, 'message', error)}")
            self.lock_id = None
        if self.hold and self.hold[1]:
            host, hold_id = self.hold
            try:
                self.gw.call("DELETE", f"/v1/hosts/{urllib.parse.quote(host)}/holds/{hold_id}",
                             timeout=30, headers={})
            except self.fail_types as error:
                self.say(f"releasing the hold: {getattr(error, 'message', error)}")
        self.hold = None

    def load(self, ident):
        try:
            return self.gw.call("POST", self.model_path(ident["target"], "load"), body={"wake": False},
                                timeout=LOAD_TIMEOUT_S, headers={})
        except self.fail_types as error:
            raise CheckError(FAIL, f"loading {ident['target']}: {getattr(error, 'message', error)}") from None

    # ------------------------------------------------------------ requests

    def body(self, ident, content, max_tokens):
        body = {"model": ident["target"], "messages": [{"role": "user", "content": content}],
                "max_tokens": max_tokens, "temperature": 0, "stream": False}
        # Fresh text already keeps the engine from reusing a prompt; this
        # also keeps the gateway from answering from a cache, on gateways
        # old (a request settings block is never cached) and new.
        block = {"settings": {"extra": {"cache_prompt": False}}}
        if self.no_cache:
            block["no_cache"] = True
        body["switchback"] = block
        return body

    def ask(self, ident, content, max_tokens, prompt_tokens_hint=0):
        body = self.body(ident, content, max_tokens)
        headers = {LOCK_HEADER: self.lock_id} if self.lock_id else {}
        deadline = time.monotonic() + PAUSED_WAIT_S * self.scale
        while True:
            started = time.monotonic()
            try:
                answer = self.gw.call("POST", "/v1/chat/completions", body=body,
                                      timeout=REQUEST_TIMEOUT_S, headers=headers)
            except self.fail_types as error:
                status = getattr(error, "status", None)
                reason = (getattr(error, "reason", ""), getattr(error, "error_type", ""))
                if status == 503 and "model_paused" in reason and time.monotonic() < deadline:
                    time.sleep(max(0.05, 5.0 * self.scale))
                    continue
                if status == 400 and "invalid_settings" in reason and self.no_cache:
                    # A gateway without `switchback.no_cache`: the settings
                    # block alone keeps its cache out.
                    self.no_cache = False
                    body = self.body(ident, content, max_tokens)
                    continue
                return {"status": status or 0, "error": str(getattr(error, "message", error))[:300],
                        "seconds": round(time.monotonic() - started, 2)}
            seconds = time.monotonic() - started
            choice = ((answer or {}).get("choices") or [{}])[0]
            message = choice.get("message") or {}
            usage = (answer or {}).get("usage") or {}
            prompt_tokens = int(usage.get("prompt_tokens") or prompt_tokens_hint or 0)
            cached = int(((usage.get("prompt_tokens_details") or {}).get("cached_tokens")) or 0)
            fresh = max(0, prompt_tokens - cached)
            # A prompt that came back faster than any engine reads it, or
            # whose tokens the engine says it had cached, was not read.
            not_read = prompt_tokens >= 2000 and (
                cached > prompt_tokens // 2 or fresh / max(seconds, 1e-6) > MAX_READ_TPS)
            return {"status": 200, "text": message.get("content") or "",
                    "reasoning_chars": len(message.get("reasoning_content") or ""),
                    "finish": choice.get("finish_reason"), "prompt_tokens": prompt_tokens,
                    "completion_tokens": int(usage.get("completion_tokens") or 0),
                    "cached_tokens": cached, "seconds": round(seconds, 2), "not_read": not_read}

    def probe(self, ident):
        """One short request with fresh codes: it loads the model if it is
        not loaded and measures how many characters a token takes."""
        rng = fresh_rng()
        prompt, codes = long_prompt(rng, PROBE_TOKENS, self.chars_per_token)
        result = self.ask(ident, prompt, LONG_MAX_TOKENS)
        if result.get("status") == 200 and result.get("prompt_tokens"):
            self.chars_per_token = max(1.5, len(prompt) / max(1, result["prompt_tokens"] - 20))
        result["result"] = classify(result, codes)
        return result

    # ------------------------------------------------------------ watching the engine

    def watch_start(self, ident):
        before = self.row_now(ident)
        state = {"rows": [before], "stop": threading.Event()}

        def loop():
            while not state["stop"].wait(WATCH_EVERY_S * self.scale):
                state["rows"].append(self.row_now(ident))

        thread = threading.Thread(target=loop, daemon=True)
        thread.start()
        state["thread"] = thread
        return state

    def row_now(self, ident):
        try:
            row, _ = self.engine_row(ident)
        except CheckError:
            return None
        return row

    def watch_stop(self, state, ident):
        state["stop"].set()
        state["thread"].join(timeout=30)
        state["rows"].append(self.row_now(ident))
        return engine_events(state["rows"])

    # ------------------------------------------------------------ the checks

    def check_long(self, ident, truth):
        ctx = self.long_ctx(truth)
        if not ctx:
            return self.cannot("long", "the model's context is unknown (no --ctx-size in its "
                                       "arguments); pass --ctx N")
        room = answer_room(ctx, LONG_MAX_TOKENS)
        tokens = int(ctx * LONG_SHARE) - room
        rng = fresh_rng()
        prompt, codes = long_prompt(rng, tokens, self.chars_per_token)
        watch = self.watch_start(ident)
        result = self.ask(ident, prompt, room, prompt_tokens_hint=tokens)
        events = self.watch_stop(watch, ident)
        result["result"] = classify(result, codes)
        result["codes_found"] = len(codes_found(result.get("text"), codes))
        return self.verdict("long", [result], events, truth, tested_ctx=ctx, sent_tokens=tokens)

    def long_ctx(self, truth):
        if getattr(self.args, "ctx", None):
            return int(self.args.ctx)
        if not truth:
            return None
        if getattr(self.args, "per_request", False):
            return truth.get("per_request_ctx")
        # The context the model is set to: what `sb ai settings` shows and
        # what callers size their requests by. A split pool fails here.
        return truth.get("ctx_size") or truth.get("per_request_ctx")

    def pool_plan(self, truth):
        """(long request tokens, [filler tokens]) that together fill the
        pool without going over it: prompts plus their answer room."""
        pool = truth.get("pool_ctx") or truth.get("ctx_size")
        per = truth.get("per_request_ctx") or pool
        slots = max(1, truth.get("slots") or 1)
        if getattr(self.args, "ctx", None):
            per = min(per or self.args.ctx, int(self.args.ctx))
        if not pool or not per:
            return None, []
        others = slots - 1
        fill_room = answer_room(per, FILL_MAX_TOKENS)
        # Room for every other slot's filler first: a pool shared by the
        # slots is no fuller with one long request than the long check.
        long_total = min(int(per * LONG_SHARE),
                         int(pool * POOL_SHARE) - others * (MIN_FILL_TOKENS + fill_room))
        long_tokens = long_total - answer_room(long_total, LONG_MAX_TOKENS)
        room = int(pool * POOL_SHARE) - long_total
        fillers = []
        if others > 0 and room > 0:
            each = min(room // others, int(per * LONG_SHARE))
            if each - fill_room >= MIN_FILL_TOKENS:
                fillers = [each - fill_room] * others
        return long_tokens, fillers

    def check_pool(self, ident, truth):
        if not truth:
            return self.cannot("pool", "the engine lists no arguments for the model")
        long_tokens, fillers = self.pool_plan(truth)
        if not long_tokens or long_tokens < MIN_FILL_TOKENS:
            return self.cannot("pool", "the model's context is unknown or too small")
        jobs = []
        per = truth.get("per_request_ctx") or truth.get("ctx_size")
        for i, tokens in enumerate([long_tokens] + fillers):
            rng = fresh_rng()
            prompt, codes = long_prompt(rng, tokens, self.chars_per_token)
            room = (answer_room(per * LONG_SHARE, LONG_MAX_TOKENS) if i == 0
                    else answer_room(per, FILL_MAX_TOKENS))
            jobs.append(("long" if i == 0 else f"fill{i}", tokens, prompt, codes, room))
        watch = self.watch_start(ident)
        results = [None] * len(jobs)

        def run(index):
            name, tokens, prompt, codes, max_tokens = jobs[index]
            # The long one first, the fillers a moment later: they all read
            # their prompts at the same time, as a busy pool does.
            time.sleep(index * 0.3 * self.scale)
            out = self.ask(ident, prompt, max_tokens, prompt_tokens_hint=tokens)
            out["name"] = name
            out["result"] = classify(out, codes)
            out["codes_found"] = len(codes_found(out.get("text"), codes))
            results[index] = out

        with ThreadPoolExecutor(max_workers=len(jobs)) as pool:
            list(pool.map(run, range(len(jobs))))
        events = self.watch_stop(watch, ident)
        total = sum(tokens + max_tokens for _, tokens, _, _, max_tokens in jobs)
        return self.verdict("pool", results, events, truth, sent_tokens=total,
                            tested_ctx=truth.get("pool_ctx") or truth.get("ctx_size"))

    def check_picture(self, ident, truth, baseline=None):
        if not (truth or {}).get("projector"):
            return {"check": "picture", "passed": None, "result": "skipped",
                    "why": "the model has no projector (no --mmproj in its arguments)"}
        runs = max(1, int(getattr(self.args, "runs", None) or PICTURE_RUNS))
        results = []
        watch = self.watch_start(ident)
        for _ in range(runs):
            rng = fresh_rng()
            image, number, shape, colour = picture(rng)
            content = [{"type": "text", "text": "What number is written in this picture, and what "
                                                "shape and colour is next to it? One short sentence."},
                       {"type": "image_url", "image_url": {
                           "url": "data:image/png;base64," + base64.b64encode(image).decode()}}]
            out = self.ask(ident, content, PICTURE_MAX_TOKENS)
            word = classify(out)
            text = (out.get("text") or "").lower()
            out["number_right"] = word == "ok" and re.search(rf"(?<!\d){number}(?!\d)", text) is not None
            out["shape_right"] = word == "ok" and shape in text
            out["colour_right"] = word == "ok" and colour in text
            if word == "ok" and not out["number_right"]:
                word = "misread"
            out["result"] = word
            results.append(out)
            self.renew()
        events = self.watch_stop(watch, ident)
        verdict = self.verdict("picture", results, events, truth)
        right = sum(1 for r in results if r["number_right"])
        answered = sum(1 for r in results if r["result"] in ("ok", "misread"))
        verdict.update(runs=runs, number_right=right,
                       shape_right=sum(1 for r in results if r["shape_right"]),
                       colour_right=sum(1 for r in results if r["colour_right"]),
                       scored_runs=answered)
        if verdict["result"] in ("request_failed", "engine_crashed", "engine_restarted",
                                 "engine_unloaded"):
            return verdict
        if answered == 0:
            verdict.update(passed=None, result="inconclusive",
                           why="no run gave an answer to score (cut at max_tokens or empty)")
            return verdict
        if baseline is not None:
            verdict["baseline"] = baseline
            base_right = baseline.get("number_right")
            if base_right is not None and right < base_right:
                verdict.update(passed=False, result="below_baseline",
                               why=f"{right}/{runs} numbers right, below {base_right}/{runs} of "
                                   f"revision {baseline.get('revision')}")
                return verdict
            verdict.update(passed=True, result="ok",
                           why=f"{right}/{runs} numbers right (revision {baseline.get('revision')}: "
                               f"{base_right}/{runs})")
            return verdict
        if right >= PICTURE_FLOOR * answered:
            verdict.update(passed=True, result="ok", why=f"{right}/{runs} numbers right")
        else:
            verdict.update(passed=False, result="misread",
                           why=f"{right}/{runs} numbers right (under {PICTURE_FLOOR:.0%} of "
                               f"{answered} answered)")
        return verdict

    def cannot(self, check, why):
        return {"check": check, "passed": None, "result": "inconclusive", "why": why, "requests": []}

    def verdict(self, check, results, events, truth, tested_ctx=None, sent_tokens=None):
        words = [r.get("result") for r in results]
        failed = [r for r in results if r.get("result") in ("request_failed", "context_exceeded")]
        out = {"check": check, "requests": results, "engine": events, "tested_ctx": tested_ctx,
               "sent_tokens": sent_tokens,
               "length_capped": words.count("length_capped"), "empty_replies": words.count("empty_reply"),
               "not_read": words.count("not_read"), "failed_requests": len(failed)}
        if events.get("crashed") or events.get("restarted") or events.get("unloaded"):
            what = ("engine_crashed" if events.get("crashed") else
                    "engine_restarted" if events.get("restarted") else "engine_unloaded")
            first = failed[0] if failed else None
            out.update(passed=False, result=what,
                       why=f"the model's process {what.split('_')[1]} during the run"
                           + (f"; {first.get('status')}: {first.get('error')}" if first else ""))
            return out
        if failed:
            first = failed[0]
            out.update(passed=False, result=first["result"],
                       why=f"{len(failed)} of {len(results)} request(s) failed; "
                           f"{first.get('status')}: {first.get('error')}")
            return out
        if check == "picture":
            out.update(passed=True, result="ok")
            return out
        if "not_read" in words:
            out.update(passed=False, result="not_read",
                       why="an answer came back faster than the prompt can be read: it came from a cache, "
                           "so the check proves nothing")
            return out
        missing = [r for r in results if r.get("result") == "codes_missing"]
        if missing:
            r = missing[0]
            out.update(passed=False, result="codes_missing",
                       why=f"{r.get('codes_found', 0)} of 3 codes came back at "
                           f"{fmt(r.get('prompt_tokens'))} tokens")
            return out
        capped = [w for w in words if w in ("length_capped", "empty_reply")]
        if capped:
            out.update(passed=None, result="length_capped" if "length_capped" in capped else "empty_reply",
                       why=f"{len(capped)} answer(s) cut at max_tokens or empty: thinking ran out of room, "
                           "not a memory or context fault; nothing to judge")
            return out
        top = max((r.get("prompt_tokens") or 0 for r in results), default=0)
        out.update(passed=True, result="ok",
                   why=f"{len(results)} request(s) answered with every code"
                       + (f" (longest {top:,} tokens)" if top else ""))
        return out

    # ------------------------------------------------------------ ledger

    def baseline(self, ident, revision, runs):
        """The newest picture result of `revision` with the same number of runs."""
        for row in self.ledger_rows(ident["target"], "check-picture"):
            if row.get("settings_revision") == revision and row.get("runs") == runs \
                    and row.get("number_right") is not None:
                return {"revision": revision, "number_right": row["number_right"], "runs": runs}
        return None

    def ledger_rows(self, target, suite=None, limit=200):
        path = "/v1/optimize/quality?model=" + urllib.parse.quote(target, safe="") + f"&limit={limit}"
        if suite:
            path += "&suite=" + urllib.parse.quote(suite)
        try:
            answer = self.gw.call("GET", path, timeout=30, headers={})
        except self.fail_types:
            return []
        return answer.get("data") or []

    def store(self, ident, verdict, note=None):
        if getattr(self.args, "no_store", False) or verdict.get("result") == "skipped":
            return None
        row = ledger_row(ident, verdict, self.truth, note)
        try:
            answer = self.gw.call("POST", "/v1/optimize/quality", body=row, timeout=30, headers={})
        except self.fail_types as error:
            self.say(f"the result was not stored: {getattr(error, 'message', error)}")
            return None
        return answer.get("id")

    # ------------------------------------------------------------ one run of checks

    def wanted(self):
        names = getattr(self.args, "checks", None) or ",".join(CHECKS)
        out = [n.strip() for n in names.split(",") if n.strip()]
        unknown = [n for n in out if n not in CHECKS]
        if unknown:
            raise CheckError(USAGE, f"unknown check {', '.join(unknown)}; checks: {', '.join(CHECKS)}")
        return out

    def run_checks(self, ident, checks, baseline=None):
        """Load the model, read its truth view, run `checks`; every result
        is stored. The machine is held and the lane locked by the caller."""
        self.load(ident)
        truth = self.read_truth(ident)
        if self.no_cache is None:
            self.no_cache = isinstance((self.row or {}).get("effective"), dict)
        self.log(f"{ident['target']} revision {ident['revision']}: {truth_words(truth)}")
        finding = split_finding(truth)
        if finding:
            self.log(f"  note: {finding}")
        probe = self.probe(ident)
        if probe.get("status") != 200:
            self.say(f"the first request failed ({probe.get('status')}: {probe.get('error')})")
        verdicts = []
        for name in checks:
            self.renew()
            self.log(f"  {name} ...")
            if name == "long":
                verdict = self.check_long(ident, truth)
            elif name == "pool":
                verdict = self.check_pool(ident, truth)
            else:
                verdict = self.check_picture(ident, truth, baseline)
            verdict["revision"] = ident["revision"]
            verdict["ledger_id"] = self.store(ident, verdict)
            verdicts.append(verdict)
            self.log("  " + verdict_line(verdict))
        return truth, verdicts


def engine_events(rows):
    """What happened to the model's process over a run, from its listing
    rows: crashed (the engine marks it failed), restarted (another process:
    a new port) or unloaded (no longer running)."""
    seen = [r for r in rows if isinstance(r, dict)]
    out = {"crashed": False, "restarted": False, "unloaded": False, "samples": len(seen)}
    if not seen:
        return out
    ports = []
    failed_before = seen[0].get("failed") not in (None, False, 0, "", {})
    for row in seen:
        status = row.get("status")
        failed = row.get("failed")
        # A failure flag left from before the run is old news; one that
        # appears during it is a crash.
        if failed not in (None, False, 0, "", {}) and not failed_before:
            out["crashed"] = True
        port = (truth_from_args(row.get("args")) or {}).get("port")
        if status in RUNNING and port:
            ports.append(port)
    if len(set(ports)) > 1:
        out["restarted"] = True
        out["ports"] = sorted(set(ports))
    first, last = seen[0].get("status"), seen[-1].get("status")
    if first in RUNNING and last not in RUNNING:
        out["unloaded"] = True
    out["status_before"], out["status_after"] = first, last
    return out


def ledger_row(ident, verdict, truth, note=None):
    """A quality-ledger row for one check: ids, counts and numbers only."""
    passed = verdict.get("passed")
    status = "complete" if passed is not None else "partial"
    row = {
        "run_id": "check_" + uuid.uuid4().hex[:16],
        "target": ident["target"],
        "suite": SUITE_PREFIX + verdict["check"],
        "suite_version": SUITE_VERSION,
        "model_file": ident.get("model_file") or UNKNOWN,
        "engine_build": ident.get("engine_build") or UNKNOWN,
        "settings_hash": ident.get("settings_hash") or UNKNOWN,
        "settings_revision": int(ident.get("revision") or 0),
        "status": status,
        "score": None,
        "se": None,
        "stop_reason": verdict.get("result") or "unknown",
        "per_case": {},
        "passed": None if passed is None else int(bool(passed)),
    }
    requests = verdict.get("requests") or []
    if verdict["check"] == "picture":
        runs = verdict.get("runs") or len(requests)
        row["score"] = (verdict.get("number_right", 0) / runs) if runs and status == "complete" else None
        row["per_case"] = {f"r{i + 1:02d}": [1.0 if r.get("number_right") else 0.0]
                           for i, r in enumerate(requests)}
        for key in ("runs", "number_right", "shape_right", "colour_right", "scored_runs"):
            row[key] = verdict.get(key)
    else:
        if status == "complete":
            row["score"] = 1.0 if passed else 0.0
        for i, r in enumerate(requests):
            name = r.get("name") or ("long" if i == 0 else f"fill{i}")
            row["per_case"][name] = [float(r.get("codes_found") or 0)]
        statuses = [r.get("status") for r in requests if r.get("status") is not None]
        row["http_status"] = max(statuses) if statuses else None
        row["codes_found"] = min((r.get("codes_found") or 0 for r in requests), default=0)
        row["sent_tokens"] = verdict.get("sent_tokens")
        row["tested_ctx"] = verdict.get("tested_ctx")
        row["read_tokens"] = max((r.get("prompt_tokens") or 0 for r in requests), default=0)
        row["requests"] = len(requests)
    for key in ("failed_requests", "length_capped", "empty_replies", "not_read"):
        row[key] = verdict.get(key, 0)
    events = verdict.get("engine") or {}
    for key in ("crashed", "restarted", "unloaded"):
        row["engine_" + key] = int(bool(events.get(key)))
    if truth:
        row["set_ctx"] = truth.get("ctx_size")
        row["per_request_ctx"] = truth.get("per_request_ctx")
        row["slots"] = truth.get("slots")
        row["kv_unified"] = None if truth.get("kv_unified") is None else int(bool(truth["kv_unified"]))
    seconds = sum(r.get("seconds") or 0 for r in requests)
    row["seconds"] = round(seconds, 1)
    if note:
        row["reverted_to"] = note.get("reverted_to")
    return {k: v for k, v in row.items() if v is not None or k in ("score", "se", "passed")}


def verdict_line(verdict):
    mark = {True: "PASS", False: "FAIL", None: "----"}[verdict.get("passed")]
    words = verdict.get("why") or verdict.get("result")
    line = f"{verdict['check']:<8} {mark}  {verdict.get('result')}: {words}"
    events = verdict.get("engine") or {}
    if events.get("ports"):
        line += f" (process ports {', '.join(str(p) for p in events['ports'])})"
    return line


def overall(verdicts):
    """True: every check that ran passed. False: one failed. None: none
    could judge."""
    judged = [v.get("passed") for v in verdicts if v.get("result") != "skipped"]
    if any(p is False for p in judged):
        return False
    if any(p is True for p in judged):
        return True if all(p is not None for p in judged) else None
    return None


def last_checks(rows, revision=None):
    """The newest check result per (revision, check) from ledger rows."""
    out = {}
    for row in rows:
        suite = str(row.get("suite") or "")
        if not suite.startswith(SUITE_PREFIX):
            continue
        rev = row.get("settings_revision")
        if revision is not None and rev != revision:
            continue
        key = (rev, suite[len(SUITE_PREFIX):])
        if key not in out or (row.get("at_ms") or 0) > (out[key].get("at_ms") or 0):
            out[key] = row
    return out


def check_words(row):
    passed = row.get("passed")
    mark = {1: "pass", 0: "FAIL", True: "pass", False: "FAIL"}.get(passed, "no verdict")
    when = time.strftime("%m-%d %H:%M", time.localtime((row.get("at_ms") or 0) / 1000))
    detail = row.get("stop_reason") or ""
    if row.get("suite") == "check-picture" and row.get("runs"):
        detail += f" {row.get('number_right')}/{row.get('runs')}"
    elif row.get("read_tokens"):
        detail += f" {row['read_tokens']:,} tokens"
    return f"{mark} ({detail.strip()}, {when})"


# ------------------------------------------------------------------ commands

def add_arguments(parser):
    parser.add_argument("model")
    parser.add_argument("--checks", default="")
    parser.add_argument("--runs", type=int, default=PICTURE_RUNS)
    parser.add_argument("--ctx", type=int)
    parser.add_argument("--per-request", action="store_true")
    parser.add_argument("--baseline", type=int)
    parser.add_argument("--wake", action="store_true")
    parser.add_argument("--no-store", action="store_true")
    parser.add_argument("--json", action="store_true")


def run(gw, args, fail_types, poll_secs=None, out=sys.stdout, err=sys.stderr):
    checker = Checker(gw, args, fail_types, out=out, err=err, poll_secs=poll_secs)
    try:
        checks = checker.wanted()
        ident = checker.resolve(args.model)
        checker.preflight(ident)
        checker.take_hold_and_lock(ident, f"sb ai check {ident['target']}")
        try:
            baseline = None
            if "picture" in checks and args.baseline is not None:
                baseline = checker.baseline(ident, args.baseline, args.runs)
                if baseline is None:
                    checker.say(f"no picture result of revision {args.baseline} with {args.runs} runs "
                                "to compare against")
            truth, verdicts = checker.run_checks(ident, checks, baseline)
        finally:
            checker.release()
    except CheckError as error:
        print(f"sb ai check: {error.message}", file=err)
        return error.code
    except KeyboardInterrupt:
        checker.release()
        print("\nsb ai check: interrupted", file=err)
        return INTERRUPTED
    verdict = overall(verdicts)
    if args.json:
        print(json.dumps({"target": ident["target"], "revision": ident["revision"],
                          "engine_build": ident.get("engine_build"), "model_file": ident.get("model_file"),
                          "truth": truth, "passed": verdict, "checks": verdicts}, indent=2), file=out)
    else:
        word = {True: "all checks passed", False: "a check failed", None: "no check could judge"}[verdict]
        print(f"{ident['target']} revision {ident['revision']}: {word}", file=out)
    return OK if verdict is not False else FAIL


def guard(gw, args, fail_types, target_view, write, poll_secs=None, out=sys.stdout, err=sys.stderr):
    """Write a guarded change and check it at once: a picture baseline of
    the current revision first (when the model has a projector and has none),
    then the write with apply now, then the checks. A failed check writes the
    previous revision's document back (the note says why). `write(extra)`
    does the settings write and returns the gateway's answer."""
    checker = Checker(gw, args, fail_types, out=out, err=err, poll_secs=poll_secs)
    try:
        checks = checker.wanted()
        ident = checker.resolve(target_view["target"])
        before_revision, before_doc = ident["revision"], ident["document"]
        checker.preflight(ident)
        checker.take_hold_and_lock(ident, f"sb ai set --check {ident['target']}")
        try:
            baseline = None
            if "picture" in checks:
                runs = int(getattr(args, "runs", None) or PICTURE_RUNS)
                baseline = checker.baseline(ident, before_revision, runs)
                if baseline is None:
                    checker.load(ident)
                    truth = checker.read_truth(ident)
                    if (truth or {}).get("projector"):
                        checker.log(f"picture baseline of revision {before_revision} first")
                        checker.no_cache = isinstance((checker.row or {}).get("effective"), dict)
                        first = checker.check_picture(ident, truth)
                        first["revision"] = before_revision
                        checker.store(ident, first)
                        checker.log("  " + verdict_line(first))
                        if first.get("number_right") is not None and first.get("scored_runs"):
                            baseline = {"revision": before_revision, "runs": runs,
                                        "number_right": first["number_right"]}
            answer = write({"apply": "now", "wake": bool(getattr(args, "wake", False))})
            apply = answer.get("apply") if isinstance(answer, dict) else None
            ident["revision"] = int(answer.get("revision") or ident["revision"])
            ident["settings_hash"] = str(answer.get("settings_hash") or ident["settings_hash"])
            checker.log(f"{ident['target']}: revision {ident['revision']}")
            if isinstance(apply, dict) and apply.get("message"):
                checker.log(apply["message"])
            failure = None
            verdicts = []
            if isinstance(apply, dict) and apply.get("ok") is False:
                failure = f"the apply failed: {apply.get('error') or apply.get('message')}"
            else:
                try:
                    truth, verdicts = checker.run_checks(ident, checks, baseline)
                except CheckError as error:
                    if error.code == OK:
                        raise
                    failure = error.message
                failed = [v for v in verdicts if v.get("passed") is False]
                if failed:
                    failure = f"{failed[0]['check']} failed ({failed[0].get('result')})"
            if failure:
                note = f"sb ai check: {failure}; back to revision {before_revision}"[:120]
                back = put_back(checker, ident, before_doc, note)
                checker.log(f"{failure}: put back the settings of revision {before_revision} "
                            f"as revision {back.get('revision')}")
                return FAIL
            word = "all checks passed" if overall(verdicts) else "no check could judge"
            checker.log(f"{ident['target']} revision {ident['revision']}: {word}; kept")
            return OK
        finally:
            checker.release()
    except CheckError as error:
        print(f"sb ai set: {error.message}", file=err)
        return error.code


def put_back(checker, ident, doc, note):
    """Write `doc` back as a new revision (apply now), with `note` in the
    history: why it was put back."""
    view = checker.call("GET", checker.model_path(ident["target"], "settings"),
                        doing="reading the settings")
    current = next((t for t in view.get("targets") or [] if t.get("target") == ident["target"]), {})
    body = dict(doc)
    body.update({"apply": "now", "note": note})
    return checker.call("PUT", checker.model_path(ident["target"], "settings"), body=body,
                        headers={"If-Match": str(current.get("revision", ident["revision"]))},
                        timeout=900, doing="putting the previous settings back")


def pending(gw, fail_types, target):
    """Guarded keys the newest revision changed whose checks have not run
    (no check result for that revision): (revision, [keys]) or None."""
    try:
        history = gw.call("GET", f"/v1/models/{urllib.parse.quote(target, safe='/')}/settings/history",
                          timeout=30, headers={})
    except fail_types:
        return None
    rows = sorted(history.get("history") or [], key=lambda r: r.get("revision") or 0, reverse=True)
    if not rows:
        return None
    newest = rows[0]
    if "sb ai check:" in str(newest.get("action") or ""):
        return None  # the checks put earlier settings back: nothing new to check
    try:
        after = json.loads(newest.get("doc") or "{}")
        before = json.loads(rows[1].get("doc") or "{}") if len(rows) > 1 else {}
    except ValueError:
        return None
    keys = guarded_changes(before, after)
    if not keys:
        return None
    checker = Checker(gw, None, fail_types)
    done = last_checks(checker.ledger_rows(target), newest.get("revision"))
    return None if done else (newest.get("revision"), keys)


def after_load(gw, args, fail_types, target, revision, keys, poll_secs=None, out=sys.stdout,
               err=sys.stderr):
    """The checks a guarded change left pending, run on the model's next
    load through `sb ai load`. A failed check writes the document of the
    revision before it back (the note says why)."""
    checker = Checker(gw, args, fail_types, out=out, err=err, poll_secs=poll_secs)
    try:
        checks = checker.wanted()
        ident = checker.resolve(target)
        if ident["revision"] != revision:
            return OK  # written again since: its own pending checks decide
        try:
            history = gw.call("GET", checker.model_path(target, "settings/history"), timeout=30,
                              headers={})
        except fail_types as error:
            raise CheckError(FAIL, f"reading the settings history: {getattr(error, 'message', error)}") from None
        rows = sorted(history.get("history") or [], key=lambda r: r.get("revision") or 0, reverse=True)
        before = next((r for r in rows if (r.get("revision") or 0) < revision), None)
        before_doc = json.loads(before.get("doc") or "{}") if before else {}
        before_revision = before.get("revision") if before else 0
        checker.log(f"{target} revision {revision} changed {', '.join(keys)}: its checks run now")
        checker.preflight(ident)
        checker.take_hold_and_lock(ident, f"sb ai load {target}: pending checks")
        try:
            runs = int(getattr(args, "runs", None) or PICTURE_RUNS)
            baseline = checker.baseline(ident, before_revision, runs) if "picture" in checks else None
            try:
                _, verdicts = checker.run_checks(ident, checks, baseline)
            except CheckError as error:
                if error.code == OK:
                    raise
                verdicts = [{"check": "run", "passed": False, "result": "request_failed",
                             "why": error.message}]
            failed = [v for v in verdicts if v.get("passed") is False]
            if failed:
                failure = f"{failed[0]['check']} failed ({failed[0].get('result')})"
                note = f"sb ai check: {failure}; back to revision {before_revision}"[:120]
                back = put_back(checker, ident, before_doc, note)
                checker.log(f"{failure}: put back the settings of revision {before_revision} "
                            f"as revision {back.get('revision')}")
                return FAIL
            checker.log(f"{target} revision {revision}: checks done; kept")
            return OK
        finally:
            checker.release()
    except CheckError as error:
        print(f"sb ai load: {error.message}", file=err)
        return error.code
