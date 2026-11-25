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

`set --load`, `load` and `sb ai tune` use the same checks as a guard: a
change to a memory or context setting must pass them, or it is put back.
Once such a change is written, checks that stop for any reason (a quiet
window, a lost lock, a machine that went off, a gateway error, Ctrl-C) put
the previous settings back too: an unchecked change never stays live.

Two more rules guard a change that makes the shared pool bigger (a larger
ctx_size or parallel, kv-unified turned on) or its cache smaller (a lower
KV cache type):

  margin   before any request: the card memory the model leaves free, from
           the engine's own memory measurement (the card's size, less this
           model, less the other models the engine has loaded, less the
           card's other users), must be at least a minimum (1.5 GB by
           default; per host `min_free_gb` in the machine file).
  repeats  the pool check runs 3 times (--pool-repeats), each on a fresh
           process of the model; one failure fails the change. An
           out-of-memory crash can depend on what else holds the card, and
           one pass missed one.

Standard library only.
"""

import base64
import json
import math
import os
import random
import re
import struct
import sys
import threading
import time
import types
import urllib.parse
import uuid
import zlib
from concurrent.futures import ThreadPoolExecutor

try:
    import ai_fit
except ImportError:  # the module sits next to this file
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import ai_fit

OK, FAIL, USAGE, MACHINE_OFF = 0, 1, 2, 3
INTERRUPTED = 130

CHECKS = ("long", "pool", "picture")
SHOWN = ("margin",) + CHECKS  # what the settings view lists per revision
SUITE_PREFIX = "check-"
# 2: the picture draws smooth pen-stroke digits (1 drew blocky 5x7 pixel digits,
# which a model misread on settings that read printed numbers right).
SUITE_VERSION = "2"
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
# The long and pool checks test memory and context, not reasoning: their
# requests ask the model not to think (`reasoning_effort: none`, which the
# gateway turns into each engine's own switch) and keep room only for the
# three code lines, so the prompts themselves fill the context or pool. A
# thinking model once spent all 4,096 answer tokens of the pool check
# thinking, twice, and the check could judge nothing. A model that thinks
# anyway (its answer carries reasoning) gets the thinking room back
# (LONG_MAX_TOKENS, FILL_MAX_TOKENS) and that check runs once more.
ANSWER_TOKENS = 512
FILL_ANSWER_TOKENS = 256
LONG_MAX_TOKENS = 4096   # room for a model that thinks anyway; a cut answer is reported as capped
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

POOL_REPEATS = 3         # pool passes a change that grows the shared pool needs
MIN_FREE_BYTES = 1_500_000_000  # card memory a change must leave free (min_free_gb per host)
GB = ai_fit.GB
# What the checks take at most, to tell before a guarded write whether they
# end before the next quiet window: a (re)load, a short first request, one
# long check, one pool check, one picture request, and putting settings back.
NEED_S = {"load": 240, "first": 60, "long": 300, "pool": 300, "picture": 30, "put_back": 300}

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

# The digits the picture check draws, as pen strokes in a box one wide and
# one high (y down), drawn thick and smooth like a plain sans-serif: a model
# that reads ordinary printed numbers must read these. Each stroke is a
# polyline ("line", points) or a piece of an ellipse ("arc", centre x, y,
# radius x, y, from and to angle in degrees; 0 is right, 90 is down).
DIGIT_STROKES = {
    "0": [("arc", 0.5, 0.5, 0.40, 0.47, 0, 360)],
    "1": [("line", [(0.22, 0.20), (0.52, 0.03), (0.52, 0.97)]),
          ("line", [(0.24, 0.97), (0.80, 0.97)])],
    "2": [("arc", 0.5, 0.30, 0.37, 0.27, -165, 25),
          ("line", [(0.835, 0.414), (0.10, 0.97), (0.92, 0.97)])],
    "3": [("arc", 0.5, 0.27, 0.34, 0.24, -150, 90),
          ("arc", 0.5, 0.74, 0.39, 0.24, -90, 150)],
    "4": [("line", [(0.70, 0.97), (0.70, 0.03), (0.06, 0.68), (0.94, 0.68)])],
    "5": [("line", [(0.86, 0.03), (0.23, 0.03), (0.19, 0.47)]),
          ("arc", 0.48, 0.67, 0.38, 0.30, -140, 140)],
    "6": [("line", [(0.80, 0.08), (0.55, 0.03), (0.30, 0.12), (0.16, 0.35), (0.13, 0.69)]),
          ("arc", 0.5, 0.69, 0.37, 0.28, 0, 360)],
    "7": [("line", [(0.08, 0.03), (0.92, 0.03), (0.38, 0.97)])],
    "8": [("arc", 0.5, 0.26, 0.31, 0.23, 0, 360),
          ("arc", 0.5, 0.73, 0.38, 0.25, 0, 360)],
    "9": [("arc", 0.5, 0.31, 0.37, 0.28, 0, 360),
          ("line", [(0.87, 0.31), (0.84, 0.65), (0.62, 0.93), (0.40, 0.98), (0.20, 0.93)])],
}
DIGIT_HEIGHT = 150      # pixels
DIGIT_WIDTH = 96
DIGIT_GAP = 28
DIGIT_STROKE = 0.065    # half the pen width, in glyph heights
DIGIT_LEFT, DIGIT_TOP = 40, 105


def digit_segments(digit, x0, y0, height=DIGIT_HEIGHT, width=DIGIT_WIDTH):
    """The pen strokes of one digit as line segments in pixels."""
    out = []
    for stroke in DIGIT_STROKES[digit]:
        if stroke[0] == "line":
            points = stroke[1]
        else:
            _, cx, cy, rx, ry, start, stop = stroke
            steps = max(8, int(abs(stop - start) / 6))
            points = [(cx + rx * math.cos(math.radians(start + (stop - start) * i / steps)),
                       cy + ry * math.sin(math.radians(start + (stop - start) * i / steps)))
                      for i in range(steps + 1)]
        pixels = [(x0 + x * width, y0 + y * height) for x, y in points]
        out.extend(zip(pixels, pixels[1:]))
    return out


def digit_pixels(digit, x0, y0, height=DIGIT_HEIGHT, width=DIGIT_WIDTH):
    """Every pixel the pen covers for one digit (a set of (x, y))."""
    half = DIGIT_STROKE * height
    covered = set()
    for (ax, ay), (bx, by) in digit_segments(digit, x0, y0, height, width):
        dx, dy = bx - ax, by - ay
        length = dx * dx + dy * dy
        for y in range(int(min(ay, by) - half) - 1, int(max(ay, by) + half) + 2):
            for x in range(int(min(ax, bx) - half) - 1, int(max(ax, bx) + half) + 2):
                t = 0.0 if length == 0 else max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / length))
                px, py = ax + t * dx - x, ay + t * dy - y
                if px * px + py * py <= half * half:
                    covered.add((x, y))
    return covered


def digit_origin(index):
    """Where the `index`-th digit of a number is drawn."""
    return DIGIT_LEFT + index * (DIGIT_WIDTH + DIGIT_GAP), DIGIT_TOP


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


def _on(value):
    return value is True or str(value).lower() in ("true", "on", "1", "yes")


def doc_pool(doc, truth=None):
    """The pool settings a settings document asks for: context, slots,
    shared pool and KV cache types. A key the document leaves out is what
    the engine runs now (`truth`), else unknown (None)."""
    load, args = (doc or {}).get("load") or {}, (doc or {}).get("engine_args") or {}
    truth = truth or {}

    def pick(load_key, arg_key, truth_key):
        value = load.get(load_key, args.get(arg_key))
        return value if value is not None else truth.get(truth_key)

    if _on(args.get("no-kv-unified")):
        unified = False
    elif "kv-unified" in args:
        unified = _on(args.get("kv-unified"))
    else:
        unified = truth.get("kv_unified")
    return {"ctx_size": _int(pick("ctx_size", "ctx-size", "ctx_size")),
            "parallel": _int(pick("parallel", "parallel", "slots")),
            "kv_unified": unified,
            "cache_type_k": pick("cache_type_k", "cache-type-k", "cache_type_k"),
            "cache_type_v": pick("cache_type_v", "cache-type-v", "cache_type_v")}


def pool_growth(before, after, truth=None):
    """How a change makes the shared pool bigger or its cache smaller, as
    short phrases (empty: it does neither). A larger ctx_size or parallel,
    kv-unified turned on, or a KV cache type with fewer bytes per element.
    `truth` (what the engine runs now) fills in what `before` leaves out; a
    key `after` sets that nothing knew before counts as a change."""
    old, new = doc_pool(before, truth), doc_pool(after, truth)
    set_before, set_after = doc_pool(before), doc_pool(after)
    out = []
    for key in ("ctx_size", "parallel"):
        if set_before[key] is not None and set_after[key] is None:
            # Unset: the engine's own value, which may be far larger.
            out.append(f"{key} {fmt(set_before[key])} -> the engine's own")
        elif new[key] is not None and (old[key] is None or new[key] > old[key]):
            out.append(f"{key} {fmt(old[key])} -> {fmt(new[key])}")
    if new["kv_unified"] is True and old["kv_unified"] is not True:
        out.append("kv-unified on")
    sizes = ai_fit.CACHE_TYPE_BYTES
    for key in ("cache_type_k", "cache_type_v"):
        was, now = (old[key] or "f16").lower(), (new[key] or "f16").lower()
        if now in sizes and sizes[now] < sizes.get(was, sizes["f16"]):
            out.append(f"{key} {was} -> {now}")
    return out


# ------------------------------------------------------------------ the memory margin

def measured(memory, pool_ctx=None):
    """The GPU bytes a model takes at its settings, from the engine's own
    memory measurement (an engine listing row's `memory`): its `effective`
    part (measured at what it runs), else its `min` part with the KV cache
    (`context_bytes`) scaled to `pool_ctx`. A dict of bytes, the context it
    stands for, the card size the measurement saw (None when the gateway
    does not pass it on) and how it was read; None when there is none."""
    if not isinstance(memory, dict):
        return None
    for name in ("effective", "min"):
        part = memory.get(name)
        if not isinstance(part, dict) or not part.get("bytes"):
            continue
        n_ctx, kv = _int(part.get("n_ctx")), part.get("context_bytes")
        card = _int(part.get("total_bytes")) or None
        if pool_ctx and n_ctx and n_ctx != pool_ctx and isinstance(kv, (int, float)):
            grown = int(part["bytes"] - kv + kv * pool_ctx / n_ctx)
            return {"bytes": grown, "n_ctx": pool_ctx, "card": card,
                    "how": f"{name} measurement at {n_ctx:,}, KV cache scaled to {pool_ctx:,}"}
        if name == "effective" or (pool_ctx and n_ctx == pool_ctx):
            return {"bytes": int(part["bytes"]), "n_ctx": n_ctx, "card": card,
                    "how": f"{name} measurement at {fmt(n_ctx)}"}
    return None


def source_words(machine):
    """` in the gateway's config` for a machine the gateway served; nothing
    for one of the local machine file (the words it always had)."""
    source = getattr(machine, "source", None)
    return f" in {source}" if source and source != "the machine file" else ""


def gateway_machines_of(engines):
    """The machines a `GET /v1/engines` answer carries (`machines`, one
    machine-file entry per host); entries that do not read are left out."""
    out = []
    for entry in (engines or {}).get("machines") or []:
        try:
            out.append(ai_fit.machine_from_entry(entry, "the gateway's machine facts"))
        except ai_fit.FitError:
            continue
    return out


def margin_verdict(numbers, min_free):
    """Pass when the card keeps at least `min_free` bytes free."""
    free = numbers["free_bytes"]
    # Unified memory is shared with every program, not a card.
    card = "memory" if "unified" in str(numbers.get("card_from") or "") else "card"
    users = "other programs" if card == "memory" else "other users of the card"
    parts = (f"{card} {gb(numbers['card_bytes'])} ({numbers['card_from']}) - model "
             f"{gb(numbers['model_bytes'])} ({numbers['model_from']})")
    if numbers.get("loaded_bytes"):
        parts += f" - other loaded models {gb(numbers['loaded_bytes'])}"
    if numbers.get("others_bytes"):
        parts += f" - {users} {gb(numbers['others_bytes'])} ({numbers['others_from']})"
    parts += f" = {gb(free)} free"
    if free < min_free:
        return {"check": "margin", "passed": False, "result": "margin_short",
                "why": f"{parts}, under the {gb(min_free)} the {'machine' if card == 'memory' else 'card'} "
                       "must keep free", "margin": numbers}
    return {"check": "margin", "passed": True, "result": "ok",
            "why": f"{parts} (at least {gb(min_free)})", "margin": numbers}


def gb(number):
    return f"{number / GB:.2f} GB" if isinstance(number, (int, float)) else "?"


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
    for n, digit in enumerate(number):
        x0, y0 = digit_origin(n)
        for x, y in digit_pixels(digit, x0, y0):
            put(x, y, (0, 0, 0))
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

def over_context(word, result, wanted, truth):
    """A 400 for a request larger than one request's context (the truth
    view's) is the context running out, whatever text the gateway passed on."""
    per = (truth or {}).get("per_request_ctx")
    if word == "request_failed" and result.get("status") == 400 and per and wanted > per:
        result["error"] = (f"{result.get('error')} (the request wanted {wanted:,} tokens; one "
                           f"request can use {per:,})")
        return "context_exceeded"
    return word


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
        self.lock_left = None   # seconds the lock was given (the next quiet window caps it)
        self.hold = None
        self.chars_per_token = FIRST_GUESS_CHARS
        self.no_cache = None  # whether the gateway takes `switchback.no_cache`
        # True once a model thought although asked not to: the long and pool
        # checks then give it the thinking room (see ANSWER_TOKENS).
        self.thinks = False
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
        model_arg = arg_value([str(a) for a in self.row.get("args") or []], ("--model", "-m"))
        if not file and model_arg:
            # The model file's identity: a hash of its path, never the path
            # (the port and the other arguments change with every load).
            file = "file-" + hex(zlib.crc32(model_arg.encode()))[2:]
        ident.update(engine_build=build, model_file=str(file or UNKNOWN),
                     file_quant=ai_fit.quant_label(str(model_arg or file or "")))
        self.truth = truth
        return truth

    # ------------------------------------------------------------ machine, hold and lock

    def lanes(self):
        return self.call("GET", "/v1/workloads/capacity", timeout=15, doing="reading the lanes").get("data") or []

    def locate(self, ident):
        """The model's host (for its machine facts), with no check of the
        lane's state: the margin before a write needs nothing else."""
        lane = next((l for l in self.lanes() if l.get("name") == ident["lane"]), None)
        if lane is not None:
            ident.setdefault("host", lane.get("host") or lane.get("name"))

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

    def take_hold_and_lock(self, ident, why, ttl=None):
        ttl = min(MAX_LOCK_S, max(MAX_LOCK_S // 4, int(ttl or 0)))
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
        # The gateway ends a lock no later than the next quiet window: a
        # lock shorter than asked for says when that window begins.
        created, expires = _int(lock.get("created_ms")), _int(lock.get("expires_at_ms"))
        if created and expires and expires - created < (ttl - 60) * 1000:
            self.lock_left = max(0, (expires - created) // 1000)

    def ends_in_time(self, need_s):
        """Raise when the next quiet window begins before checks that take
        `need_s` seconds can end (nothing has been written yet)."""
        if self.lock_left is not None and self.lock_left < need_s:
            raise CheckError(FAIL, f"the quiet window begins in {self.lock_left // 60} min and the "
                                   f"checks need up to {math.ceil(need_s / 60)} min; nothing was "
                                   "written (try after the window)")

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
            # Not a pass: the checks did not end. A guarded change is put
            # back by the caller.
            raise CheckError(FAIL, f"the quiet window {beat['quiet_window']} began; checks stopped")

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

    def fresh_process(self, ident):
        """Unload the model and load it again: a new process, whose first
        requests meet the card as a fresh load does (an out-of-memory crash
        came on a fresh process's first requests)."""
        try:
            self.gw.call("POST", self.model_path(ident["target"], "unload"), body={},
                         timeout=LOAD_TIMEOUT_S, headers={})
        except self.fail_types as error:
            raise CheckError(FAIL, f"unloading {ident['target']}: {getattr(error, 'message', error)}") from None
        self.load(ident)
        return self.read_truth(ident)

    # ------------------------------------------------------------ the card's memory

    def machine(self, ident):
        """The entry for the model's host (or lane): from the local machine
        file (--machines, else SB_AI_MACHINES) when there is one, else the
        gateway's own machine facts (`server.optimize.hosts` in its config,
        served in `GET /v1/engines`); None when neither knows it."""
        path = getattr(self.args, "machines", None)
        names = (ident.get("host"), ident.get("lane"))
        if path:
            try:
                machines = ai_fit.read_machines(path)
            except ai_fit.FitError as error:
                raise CheckError(USAGE, error.message) from None
            found = next((m for m in machines if m.name in names), None)
            if found is not None:
                found.source = "the machine file"
                return found
            # The local file does not name this host: the gateway's facts
            # still apply.
        machines = self.gateway_machines()
        found = next((m for m in machines if m.name in names), None)
        if found is not None:
            found.source = "the gateway's config"
        return found

    def gateway_machines(self):
        """The gateway's machine facts as machines (empty when it serves
        none, or this key may not read them)."""
        if "_gateway_machines" not in self.__dict__:
            try:
                answer = self.gw.get("/v1/engines")
            except self.fail_types:
                answer = {}
            self._gateway_machines = gateway_machines_of(answer)
        return self._gateway_machines

    def fit_estimate(self, ident, truth, machine):
        """The model's GPU bytes at its settings from `sb ai fit`'s numbers,
        when the engine measures nothing: the weights of the running file's
        quant (the model index's file sizes), the KV cache of the whole pool
        at its cache types (the file header's layers and heads), the
        projector when it reads pictures, and the runtime's compute buffers
        (the host's compute_gb, else fit's 1.5 GB). The repository is the
        one the settings' fit ceiling names (`sb ai fit REPO --store`).
        Returns (numbers, None) or (None, why not)."""
        source = str(((ident.get("document") or {}).get("ceiling") or {}).get("source") or "")
        found = re.match(r"sb ai fit (\S+/\S+)(?: (\S+))?", source)
        if not found:
            return None, (f"its model file is unknown to fit (run `sb ai fit REPO --store "
                          f"{ident['target']}` once)")
        repo = found.group(1)
        quant = ident.get("file_quant") or found.group(2)
        ctx = (truth or {}).get("ctx_size")
        if not quant or not ctx:
            return None, "the running file's quant or the pool's context is unknown"
        k = str((truth or {}).get("cache_type_k") or "f16").lower()
        v = str((truth or {}).get("cache_type_v") or "f16").lower()
        key = (repo, ctx, k, v)
        cache = self.__dict__.setdefault("_fits", {})
        if key not in cache:
            args = types.SimpleNamespace(repo=repo, ctx=int(ctx), cache_type_k=k, cache_type_v=v,
                                         machines=None, vram=None, ram=None, others=None, compute=None)
            try:
                cache[key] = ai_fit.build_report(args, {"HF_TOKEN": os.environ.get("HF_TOKEN", "")})
            except ai_fit.FitError as error:
                return None, f"fit could not read {repo} ({error.message})"
        report = cache[key]
        option = next((o for o in report.get("options") or []
                       if str(o.get("quant") or "").upper() == quant.upper()), None)
        kv = report.get("kv_cache") or {}
        if option is None or not option.get("bytes"):
            return None, f"fit finds no {quant} file in {repo}"
        if not kv.get("known"):
            return None, f"fit cannot size the KV cache of {repo} from its header"
        projector = int((report.get("projector") or {}).get("bytes") or 0) \
            if (truth or {}).get("projector") else 0
        compute = int(machine.compute) if machine is not None else ai_fit.MARGIN_BYTES
        total = int(option["bytes"]) + int(kv["bytes"]) + projector + compute
        how = (f"fit {repo} {quant}: weights {gb(option['bytes'])} + KV {gb(kv['bytes'])} at "
               f"{int(ctx):,} {k}/{v}" + (f" + projector {gb(projector)}" if projector else "")
               + f" + compute {gb(compute)}")
        return {"bytes": total, "n_ctx": int(ctx), "card": None, "how": how}, None

    def predicted_margin(self, ident, truth_before, after_doc):
        """The margin rule at the settings a change asks for, before anything
        is written, held or loaded: the pool the document asks for (what it
        leaves out is what the engine runs now), sized from the engine's
        measurement with its KV cache scaled (same cache types), else from
        `sb ai fit`'s numbers. None when nothing can size it (the normal
        checks then judge it after the load)."""
        if not truth_before:
            return None
        if not ident.get("file_quant"):
            # The quant of the file the engine runs (its listed --model), as
            # the checks after a load read it.
            row, _ = self.engine_row(ident)
            model_arg = arg_value([str(a) for a in (row or {}).get("args") or []], ("--model", "-m"))
            if model_arg:
                ident["file_quant"] = ai_fit.quant_label(str(model_arg))
        pool = doc_pool(after_doc, truth_before)
        if not pool.get("ctx_size"):
            return None
        predicted = dict(truth_before)
        predicted.update({"ctx_size": pool["ctx_size"], "pool_ctx": pool["ctx_size"],
                          "cache_type_k": pool["cache_type_k"] or truth_before.get("cache_type_k"),
                          "cache_type_v": pool["cache_type_v"] or truth_before.get("cache_type_v"),
                          "kv_unified": pool["kv_unified"],
                          "slots": pool["parallel"] or truth_before.get("slots")})
        same_cache = all(str(predicted.get(k) or "f16").lower() == str(truth_before.get(k) or "f16").lower()
                         for k in ("cache_type_k", "cache_type_v"))
        verdict = self.check_margin(ident, predicted, use_measurement=same_cache)
        if verdict.get("passed") is None:
            return None
        return verdict

    def check_margin(self, ident, truth, use_measurement=True):
        """Before any request: the card memory the model leaves free at its
        settings, from the engine's own measurement. The card's size is the
        measurement's (when the gateway passes it on), else --card-gb, else
        the host's vram_gb in the machine file; other users of the card
        (a desktop, an image server) are --others, else the host's
        others_gb; the minimum is --min-free, else the host's min_free_gb,
        else 1.5 GB."""
        engines = self.call("GET", "/v1/engines", timeout=30, doing="reading the engines")
        rows = next((lane.get("models") or [] for lane in engines.get("data") or []
                     if lane.get("lane") == ident["lane"]), [])
        row = next((r for r in rows if r.get("id") == ident["model"]), None)
        pool_ctx = (truth or {}).get("pool_ctx") or (truth or {}).get("ctx_size")
        machine = self.machine(ident)
        where = f"host {ident.get('host') or ident['lane']}"
        mine = measured((row or {}).get("memory"), pool_ctx) if use_measurement else None
        if mine is None:
            mine, why = self.fit_estimate(ident, truth, machine)
            if mine is None:
                return {"check": "margin", "passed": None, "result": "skipped",
                        "why": f"the engine reports no memory measurement for the model, and {why}"}
        card_flag = getattr(self.args, "card_gb", None)
        if card_flag:
            card, card_from = int(float(card_flag) * GB), "--card-gb"
        elif mine["card"]:
            card, card_from = mine["card"], "the engine's measurement"
        elif machine is not None and machine.budget:
            name = "vram_gb" if machine.kind == "gpu" else "usable_gb (unified memory)"
            card, card_from = int(machine.budget), f"{name} of {where} in " \
                f"{getattr(machine, 'source', None) or 'the machine file'}"
        else:
            return {"check": "margin", "passed": None, "result": "skipped",
                    "why": (f"the card's size is unknown: the gateway does not pass it on, so set "
                            f"vram_gb for {where} in the gateway's server.optimize.hosts or in a "
                            "machine file (SB_AI_MACHINES), or pass --card-gb")}
        loaded, unknown = 0, []
        for other in rows:
            # A sleeping model's process holds no weights (llama.cpp frees
            # them while it sleeps), so only loaded and loading ones count.
            if other is row or other.get("status") not in ("loaded", "loading"):
                continue
            theirs = measured(other.get("memory"))
            if theirs is None:
                unknown.append(str(other.get("id")))
            else:
                loaded += theirs["bytes"]
        in_others = []
        if unknown and machine is not None and machine.kind == "unified" and machine.others:
            # On unified memory, others_gb is everything but this model at the
            # machine's normal state, measured with its small always-loaded
            # models in: an unmeasured loaded model is counted there (and named).
            in_others, unknown = unknown, []
        if unknown:
            # Their share of the card is not known: the room left is not either.
            return {"check": "margin", "passed": None, "result": "skipped",
                    "why": (f"{', '.join(unknown[:3])} {'is' if len(unknown) == 1 else 'are'} loaded "
                            "on the same engine with no memory measurement, so the room left is "
                            "unknown")}
        others_flag = getattr(self.args, "others", None)
        if others_flag is not None:
            others, others_from = int(float(others_flag) * GB), "--others"
        elif machine is not None and machine.others:
            others, others_from = int(machine.others), f"others_gb of {where}{source_words(machine)}"
            if in_others:
                others_from += f", with {', '.join(in_others[:3])} loaded"
        else:
            others, others_from = 0, "none named"
        free_flag = getattr(self.args, "min_free", None)
        if free_flag is not None:
            min_free, min_free_from = int(float(free_flag) * GB), "--min-free"
        elif machine is not None and getattr(machine, "min_free", None) is not None:
            min_free = int(machine.min_free)
            min_free_from = f"min_free_gb of {where}{source_words(machine)}"
        else:
            min_free, min_free_from = MIN_FREE_BYTES, "the default"
        numbers = {"card_bytes": card, "card_from": card_from, "model_bytes": mine["bytes"],
                   "model_from": mine["how"], "measured_ctx": mine["n_ctx"], "loaded_bytes": loaded,
                   "others_bytes": others, "others_from": others_from,
                   "free_bytes": card - mine["bytes"] - loaded - others, "min_free_bytes": min_free,
                   "min_free_from": min_free_from}
        return margin_verdict(numbers, min_free)

    # ------------------------------------------------------------ requests

    def long_room(self):
        """Most answer tokens a long request (and the probe) gets."""
        return LONG_MAX_TOKENS if self.thinks else ANSWER_TOKENS

    def fill_room(self):
        """Most answer tokens a pool filler gets."""
        return FILL_MAX_TOKENS if self.thinks else FILL_ANSWER_TOKENS

    def body(self, ident, content, max_tokens, think=True):
        """A check request. `think` False: the model is asked not to think
        (memory and context checks; the picture check keeps the model's way)."""
        body = {"model": ident["target"], "messages": [{"role": "user", "content": content}],
                "max_tokens": max_tokens, "temperature": 0, "stream": False}
        if not think:
            body["reasoning_effort"] = "none"
        # Fresh text already keeps the engine from reusing a prompt; this
        # also keeps the gateway from answering from a cache, on gateways
        # old (a request settings block is never cached) and new.
        block = {"settings": {"extra": {"cache_prompt": False}}}
        if self.no_cache:
            block["no_cache"] = True
        body["switchback"] = block
        return body

    def ask(self, ident, content, max_tokens, prompt_tokens_hint=0, think=True):
        body = self.body(ident, content, max_tokens, think)
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
                    body = self.body(ident, content, max_tokens, think)
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
        result = self.ask(ident, prompt, self.long_room(), think=False)
        if result.get("status") == 200 and result.get("prompt_tokens"):
            self.chars_per_token = max(1.5, len(prompt) / max(1, result["prompt_tokens"] - 20))
        result["result"] = classify(result, codes)
        self.learn_thinking([result])
        return result

    def learn_thinking(self, results):
        """True when an answer asked not to think carried reasoning: from
        now on the long and pool checks give the model the thinking room."""
        if self.thinks or not any((r or {}).get("reasoning_chars") for r in results):
            return False
        self.thinks = True
        self.say(f"{(self.ident or {}).get('target', 'the model')} thinks although asked not to: "
                 f"the long and pool checks give it {LONG_MAX_TOKENS:,} answer tokens")
        return True

    def thinking_rerun(self, verdict):
        """A check whose answers were cut while the model thought although
        asked not to: worth one more run with the thinking room."""
        if verdict.get("result") != "length_capped":
            return False
        return self.learn_thinking(verdict.get("requests") or [])

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
        verdict = self.check_long_once(ident, truth)
        if self.thinking_rerun(verdict):
            verdict = self.check_long_once(ident, truth)
        return verdict

    def check_long_once(self, ident, truth):
        ctx = self.long_ctx(truth)
        if not ctx:
            return self.cannot("long", "the model's context is unknown (no --ctx-size in its "
                                       "arguments); pass --ctx N")
        room = answer_room(ctx, self.long_room())
        tokens = int(ctx * LONG_SHARE) - room
        rng = fresh_rng()
        prompt, codes = long_prompt(rng, tokens, self.chars_per_token)
        watch = self.watch_start(ident)
        result = self.ask(ident, prompt, room, prompt_tokens_hint=tokens, think=False)
        events = self.watch_stop(watch, ident)
        result["result"] = over_context(classify(result, codes), result, tokens + room, truth)
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
        fill_room = answer_room(per, self.fill_room())
        # Room for every other slot's filler first: a pool shared by the
        # slots is no fuller with one long request than the long check.
        long_total = min(int(per * LONG_SHARE),
                         int(pool * POOL_SHARE) - others * (MIN_FILL_TOKENS + fill_room))
        long_tokens = long_total - answer_room(long_total, self.long_room())
        room = int(pool * POOL_SHARE) - long_total
        fillers = []
        if others > 0 and room > 0:
            each = min(room // others, int(per * LONG_SHARE))
            if each - fill_room >= MIN_FILL_TOKENS:
                fillers = [each - fill_room] * others
        return long_tokens, fillers

    def check_pool(self, ident, truth):
        verdict = self.check_pool_once(ident, truth)
        if self.thinking_rerun(verdict):
            verdict = self.check_pool_once(ident, truth)
        return verdict

    def check_pool_once(self, ident, truth):
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
            room = (answer_room(per * LONG_SHARE, self.long_room()) if i == 0
                    else answer_room(per, self.fill_room()))
            jobs.append(("long" if i == 0 else f"fill{i}", tokens, prompt, codes, room))
        watch = self.watch_start(ident)
        results = [None] * len(jobs)

        def run(index):
            name, tokens, prompt, codes, max_tokens = jobs[index]
            # The long one first, the fillers a moment later: they all read
            # their prompts at the same time, as a busy pool does.
            time.sleep(index * 0.3 * self.scale)
            out = self.ask(ident, prompt, max_tokens, prompt_tokens_hint=tokens, think=False)
            out["name"] = name
            out["result"] = over_context(classify(out, codes), out, tokens + max_tokens, truth)
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
                       why=f"{len(capped)} answer(s) cut at max_tokens or empty: thinking ran out of room "
                           f"({'with' if self.thinks else 'asked not to think, without'} the thinking room), "
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
                    and row.get("suite_version") == SUITE_VERSION \
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
        """The checks --checks names (all of them by default), the margin
        rule among them."""
        names = getattr(self.args, "checks", None) or ",".join(SHOWN)
        out = [n.strip() for n in names.split(",") if n.strip()]
        unknown = [n for n in out if n not in SHOWN]
        if unknown:
            raise CheckError(USAGE, f"unknown check {', '.join(unknown)}; checks: {', '.join(SHOWN)}")
        return out

    def record(self, ident, verdict, verdicts):
        verdict["revision"] = ident["revision"]
        verdict["ledger_id"] = self.store(ident, verdict)
        verdicts.append(verdict)
        self.log("  " + verdict_line(verdict))
        return verdict

    def margin_step(self, ident, truth, verdicts, required=False):
        """The margin rule on the process now loaded; True when it failed,
        or (`required`: a change that grows the pool) could not judge. Then
        no request is sent."""
        verdict = self.check_margin(ident, truth)
        self.record(ident, verdict, verdicts)
        if verdict.get("passed") is None and required:
            self.say(f"a larger pool needs the margin rule: {verdict.get('why')}")
            return True
        return verdict.get("passed") is False

    def run_checks(self, ident, checks, baseline=None, pool_repeats=1, margin=True):
        """Load the model, read its truth view, run `checks`; every result
        is stored. The machine is held and the lane locked by the caller.
        `margin`: the margin rule first, before any request (a failure ends
        the run there; "required": one that cannot judge too). `pool_repeats`: the pool check that many times, the
        later ones each on a fresh process; the first that does not pass
        ends the repeats."""
        self.load(ident)
        truth = self.read_truth(ident)
        if self.no_cache is None:
            self.no_cache = isinstance((self.row or {}).get("effective"), dict)
        self.log(f"{ident['target']} revision {ident['revision']}: {truth_words(truth)}")
        finding = split_finding(truth)
        if finding:
            self.log(f"  note: {finding}")
        verdicts = []
        required = margin == "required"
        if margin and self.margin_step(ident, truth, verdicts, required):
            return truth, verdicts
        watch = self.watch_start(ident)
        probe = self.probe(ident)
        events = self.watch_stop(watch, ident)
        if probe.get("status") != 200:
            self.say(f"the first request failed ({probe.get('status')}: {probe.get('error')})")
            if events.get("crashed") or events.get("restarted") or events.get("unloaded"):
                # The model's process died on a short first request: every
                # check fails here (the gateway now cools the lane down, so a
                # check sent next would only meet that).
                probe["name"], probe["result"] = "first", "request_failed"
                for name in checks:
                    verdict = self.verdict(name, [probe], events, truth)
                    verdict["why"] = (f"the model's process {verdict['result'].split('_')[1]} on the "
                                      f"first, short request ({probe.get('status')}: "
                                      f"{probe.get('error')})")
                    verdict["revision"] = ident["revision"]
                    verdict["ledger_id"] = self.store(ident, verdict)
                    verdicts.append(verdict)
                    self.log("  " + verdict_line(verdict))
                return truth, verdicts
        repeats = max(1, int(pool_repeats or 1))
        for name in checks:
            self.renew()
            if name == "pool":
                for n in range(repeats):
                    if n:
                        self.renew()
                        self.log(f"  pool {n + 1}/{repeats}: unload and load again (a fresh process) ...")
                        truth = self.fresh_process(ident)
                        if margin and self.margin_step(ident, truth, verdicts, required):
                            break
                    else:
                        self.log(f"  pool{f' 1/{repeats}' if repeats > 1 else ''} ...")
                    verdict = self.check_pool(ident, truth)
                    if repeats > 1:
                        verdict.update(repeat=n + 1, repeats=repeats)
                    self.record(ident, verdict, verdicts)
                    if verdict.get("passed") is not True:
                        break
                continue
            self.log(f"  {name} ...")
            if name == "long":
                verdict = self.check_long(ident, truth)
            else:
                verdict = self.check_picture(ident, truth, baseline)
            self.record(ident, verdict, verdicts)
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
    if verdict["check"] == "margin":
        numbers = verdict.get("margin") or {}
        if status == "complete":
            row["score"] = 1.0 if passed else 0.0
        for key in ("card_bytes", "model_bytes", "loaded_bytes", "others_bytes", "free_bytes",
                    "min_free_bytes", "measured_ctx"):
            if isinstance(numbers.get(key), int):
                row[key] = numbers[key]
    elif verdict["check"] == "picture":
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
    for key in ("repeat", "repeats"):
        if isinstance(verdict.get(key), int):
            row[key] = verdict[key]
    seconds = sum(r.get("seconds") or 0 for r in requests)
    row["seconds"] = round(seconds, 1)
    if note:
        row["reverted_to"] = note.get("reverted_to")
    return {k: v for k, v in row.items() if v is not None or k in ("score", "se", "passed")}


def verdict_line(verdict):
    mark = {True: "PASS", False: "FAIL", None: "----"}[verdict.get("passed")]
    words = verdict.get("why") or verdict.get("result")
    name = verdict["check"]
    if verdict.get("repeats"):
        name += f" {verdict.get('repeat')}/{verdict['repeats']}"
    line = f"{name:<8} {mark}  {verdict.get('result')}: {words}"
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
    elif row.get("suite") == "check-margin" and isinstance(row.get("free_bytes"), int):
        detail += f" {gb(row['free_bytes'])} free"
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
    add_guard_arguments(parser)


def add_guard_arguments(parser):
    """The flags of the margin rule and the pool repeats (check, set, load)."""
    parser.add_argument("--pool-repeats", type=int)
    parser.add_argument("--min-free", type=float)
    parser.add_argument("--others", type=float)
    parser.add_argument("--card-gb", type=float)


def pool_repeats_for(args, growth):
    """--pool-repeats, else POOL_REPEATS for a change that grows the pool,
    else one."""
    asked = getattr(args, "pool_repeats", None)
    if asked is not None:
        if asked < 1:
            raise CheckError(USAGE, "--pool-repeats takes a number of 1 or more")
        return asked
    return POOL_REPEATS if growth else 1


def need_s(checks, repeats, runs, projector=True, baseline=False):
    """The most the checks of a guarded change take, in seconds: the load,
    the first request, each check (the pool once per repeat, the later
    repeats each with an unload and a load), the picture runs (twice
    without a stored baseline), and putting the settings back."""
    total = NEED_S["load"] + NEED_S["first"] + NEED_S["put_back"]
    if "long" in checks:
        total += NEED_S["long"]
    if "pool" in checks:
        total += repeats * NEED_S["pool"] + (repeats - 1) * 2 * NEED_S["load"]
    if "picture" in checks and projector is not False:
        total += runs * NEED_S["picture"] * (1 if baseline else 2)
    return total


def run(gw, args, fail_types, poll_secs=None, out=sys.stdout, err=sys.stderr):
    checker = Checker(gw, args, fail_types, out=out, err=err, poll_secs=poll_secs)
    try:
        checks = checker.wanted()
        repeats = pool_repeats_for(args, [])
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
            truth, verdicts = checker.run_checks(ident, [c for c in checks if c != "margin"], baseline,
                                                 pool_repeats=repeats, margin="margin" in checks)
        finally:
            checker.release()
    except CheckError as error:
        print(f"sb ai check: {error.message}", file=err)
        return error.code or FAIL
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


def change_failure(verdicts, checks, growth, repeats):
    """Why the checks of a change do not let it stay, or None. A failed
    check fails it; a change that grows the shared pool also needs every
    pool repeat to pass (one that cannot judge does not approve it)."""
    failed = [v for v in verdicts if v.get("passed") is False]
    unjudged = [v for v in verdicts if v.get("check") == "margin" and v.get("passed") is None]
    if growth and unjudged and not failed:
        return "margin could not judge (a larger pool needs it)"
    if failed:
        first = failed[0]
        name = first["check"]
        if first.get("repeats"):
            name += f" {first.get('repeat')}/{first['repeats']}"
        return f"{name} failed ({first.get('result')})"
    if growth and "pool" in checks:
        passed = sum(1 for v in verdicts if v.get("check") == "pool" and v.get("passed") is True)
        if passed < repeats:
            return f"pool passed {passed} of {repeats} (a larger pool needs every one)"
    return None


def keep_or_put_back(checker, ident, before_revision, before_doc, checks, growth, repeats, run_them):
    """Run the checks of a change that is live now (`run_them()` returns
    (truth, verdicts)) and keep it only when they pass. A failed check, or
    checks that stop for any reason (a quiet window, a lost lock, a machine
    gone off, a gateway error, Ctrl-C), write the previous revision's
    document back, with the reason in the history note. Returns the exit
    code: 0 only when the change was kept."""
    verdicts, failure, code = [], None, FAIL
    try:
        _, verdicts = run_them()
    except CheckError as error:
        failure, code = f"checks stopped: {error.message}", error.code or FAIL
    except KeyboardInterrupt:
        failure, code = "checks stopped: interrupted", INTERRUPTED
    except Exception as error:  # noqa: BLE001 - an unchecked change must never stay live
        failure = f"checks stopped: {type(error).__name__}: {error}"
    if failure is None:
        failure = change_failure(verdicts, checks, growth, repeats)
    if failure is None:
        word = "all checks passed" if overall(verdicts) else "no check could judge"
        checker.log(f"{ident['target']} revision {ident['revision']}: {word}; kept")
        return OK
    note = f"sb ai check: {failure}; back to revision {before_revision}"[:120]
    try:
        back = put_back(checker, ident, before_doc, note)
    except (CheckError, KeyboardInterrupt) as error:
        why = getattr(error, "message", "interrupted")
        checker.say(f"{failure}; putting the settings of revision {before_revision} back failed "
                    f"({why}): revision {ident['revision']} is live and unchecked; run "
                    f"`sb ai revert {ident['target']} {before_revision}`")
        return code
    done = (f"{failure}: put back the settings of revision {before_revision} "
            f"as revision {back.get('revision')}")
    if getattr(checker.args, "json", False):
        checker.say(done)
    else:
        checker.log(done)
    apply = back.get("apply") if isinstance(back, dict) else None
    if isinstance(apply, dict) and apply.get("pending"):
        checker.say("the model is busy: the gateway applies the old settings once its requests end")
    elif isinstance(apply, dict) and apply.get("ok") is False:
        checker.say(f"the old settings are saved but not applied yet: {apply.get('error') or apply.get('message')}")
    return code


def guard(gw, args, fail_types, target_view, write, after_doc=None, poll_secs=None, out=sys.stdout,
          err=sys.stderr):
    """Write a guarded change and check it at once: a picture baseline of
    the current revision first (when the model has a projector and has none),
    then the write with apply now, then the checks. A failed check, or
    checks that stop for any reason after the write, write the previous
    revision's document back (the note says why). Nothing is written when
    the checks cannot end before the next quiet window. `write(extra)` does
    the settings write and returns the gateway's answer; `after_doc` is the
    document it writes."""
    checker = Checker(gw, args, fail_types, out=out, err=err, poll_secs=poll_secs)
    try:
        checks = checker.wanted()
        ident = checker.resolve(target_view["target"])
        before_revision, before_doc = ident["revision"], ident["document"]
        row, _ = checker.engine_row(ident)
        truth_before = truth_of(row or {})
        growth = pool_growth(before_doc, after_doc, truth_before) if after_doc is not None else []
        if growth:
            # The margin needs no request and no load: a change that leaves too
            # little free is refused before anything is written, held, woken
            # or loaded (on a work machine, a too-big model would otherwise load
            # first and squeeze everything else until the check puts it back).
            checker.locate(ident)
            early = checker.predicted_margin(ident, truth_before, after_doc)
            if early is not None:
                checker.log(f"{ident['target']}: at the new settings ({', '.join(growth)}), before any "
                            f"write: {verdict_line(early)}")
                if early.get("passed") is False:
                    raise CheckError(FAIL, f"{ident['target']}: the change would leave too little memory "
                                           "free; nothing was written, held, woken or loaded")
        try:
            checker.preflight(ident)
        except CheckError as error:
            if error.code == MACHINE_OFF:
                raise CheckError(MACHINE_OFF, f"{error.message}, or --no-check to keep the change "
                                              "for the next load, unchecked") from None
            raise
        repeats = pool_repeats_for(args, growth)
        runs = int(getattr(args, "runs", None) or PICTURE_RUNS)
        need = need_s(checks, repeats, runs, projector=(truth_before or {}).get("projector"))
        checker.take_hold_and_lock(ident, f"sb ai set --check {ident['target']}", ttl=need + 600)
        try:
            checker.ends_in_time(need)
            if growth:
                checker.log(f"{ident['target']}: the change grows the shared pool ({', '.join(growth)}): "
                            f"margin rule, then {repeats} pool checks, each after the first on a fresh "
                            "process")
            baseline = None
            if "picture" in checks:
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
            # Written: from here on the change is live, and only checks that
            # pass keep it.
            apply = answer.get("apply") if isinstance(answer, dict) else None
            ident["revision"] = int(answer.get("revision") or ident["revision"])
            ident["settings_hash"] = str(answer.get("settings_hash") or ident["settings_hash"])
            checker.log(f"{ident['target']}: revision {ident['revision']}")
            if isinstance(apply, dict) and apply.get("message"):
                checker.log(apply["message"])

            def run_them():
                if isinstance(apply, dict) and apply.get("ok") is False:
                    raise CheckError(FAIL, f"the apply failed: {apply.get('error') or apply.get('message')}")
                return checker.run_checks(ident, [c for c in checks if c != "margin"], baseline,
                                          pool_repeats=repeats,
                                          margin="required" if growth else "margin" in checks)

            return keep_or_put_back(checker, ident, before_revision, before_doc, checks, growth,
                                    repeats, run_them)
        finally:
            checker.release()
    except CheckError as error:
        print(f"sb ai set: {error.message}", file=err)
        return error.code or FAIL


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


def history_doc(row):
    """A settings history row's document (`document`, an object)."""
    doc = row.get("document")
    return doc if isinstance(doc, dict) else {}


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
    after = history_doc(newest)
    before = history_doc(rows[1]) if len(rows) > 1 else {}
    keys = guarded_changes(before, after)
    if not keys:
        return None
    checker = Checker(gw, None, fail_types)
    ledger = checker.ledger_rows(target)
    if last_checks(ledger, newest.get("revision")):
        return None
    # The same memory and context settings under an earlier revision (a
    # revert to them) whose long check passed: nothing new to check.
    same = {row.get("revision") for row in rows[1:]
            if not guarded_changes(history_doc(row), after)}
    passed = last_checks(ledger)
    if any(rev in same and check == "long" and row.get("passed") in (1, True)
           for (rev, check), row in passed.items()):
        return None
    return newest.get("revision"), keys


def after_load(gw, args, fail_types, target, revision, keys, poll_secs=None, out=sys.stdout,
               err=sys.stderr):
    """The checks a guarded change left pending, run on the model's next
    load through `sb ai load`. The change is live once loaded, so a failed
    check, or checks that cannot start or stop for any reason, write the
    document of the revision before it back (the note says why)."""
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
        before_doc = history_doc(before) if before else {}
        before_revision = before.get("revision") if before else 0
        growth = pool_growth(before_doc, ident["document"])
        repeats = pool_repeats_for(args, growth)
        runs = int(getattr(args, "runs", None) or PICTURE_RUNS)
        checker.log(f"{target} revision {revision} changed {', '.join(keys)}: its checks run now")
        if growth:
            checker.log(f"{target}: the change grows the shared pool ({', '.join(growth)}): margin "
                        f"rule, then {repeats} pool checks, each after the first on a fresh process")

        def run_them():
            checker.preflight(ident)
            baseline = checker.baseline(ident, before_revision, runs) if "picture" in checks else None
            need = need_s(checks, repeats, runs, projector=(checker.truth or {}).get("projector"),
                          baseline=baseline is not None)
            checker.take_hold_and_lock(ident, f"sb ai load {target}: pending checks", ttl=need + 600)
            checker.ends_in_time(need)
            return checker.run_checks(ident, [c for c in checks if c != "margin"], baseline,
                                      pool_repeats=repeats,
                                      margin="required" if growth else "margin" in checks)

        try:
            return keep_or_put_back(checker, ident, before_revision, before_doc, checks, growth,
                                    repeats, run_them)
        finally:
            checker.release()
    except CheckError as error:
        print(f"sb ai load: {error.message}", file=err)
        return error.code or FAIL
