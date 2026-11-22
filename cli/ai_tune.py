#!/usr/bin/env python3
"""sb ai tune: controlled speed experiments on one local model, through the
gateway.

The loop of the inference-optimisation skill
(https://github.com/sybil-solutions/inference-optimisation-skill), run on a
live gateway: measure a baseline, read the ceiling, change one load setting
at a time from the current best, and judge each change with
control / candidate / control runs. Before the first lever it measures the
noise: three runs of the same settings (five, with more samples per run
when the budget allows, when those differ by more than 2%), measuring every
metric the planned levers use. The noise is the standard deviation (SD) of
one run, per metric, never taken under 2%; an SD over 20% stops the run
with "too noisy to tune" and changes nothing. Each change then runs
control, candidate, control, candidate, control... (each candidate run
between two controls) until the result is clear, at most ten candidate
runs: kept when its gain over the controls' mean is larger than 1.7 SD
with two standard errors to spare (or, after the last run, larger than
both 1.7 SD and two standard errors), rejected as soon as the gain plus
two standard errors stays under 1.7 SD. The SD is pooled over the noise
runs, the controls and the candidate runs (so an unsteady candidate widens
it). On a machine noisier than 5%, or for a win over 20%, a win must hold
over two candidate runs or more. No other measured metric may get worse
than the controls' mean by more than such a win would need on it. A trial
whose settings do not apply or load is a failed trial: shown as such, not a
rejection, and not a lever inside the noise. A change that cannot finish
inside the budget is not started; one the budget cuts off changes nothing.
Changes that claim to be lossless must give the same greedy answer; changes
to the KV cache or context must find needles at about half and all of the
context; a change to the slot count must survive a concurrent soak at full
per-slot context. The run stops after three levers in a row inside the
noise, at the end of its budget, when a quiet window begins, when its tuning
lock is gone (the gateway restarted), or on Ctrl-C, SIGTERM or SIGHUP, and
always leaves the best known settings applied.

Safety: the run never starts on a sleeping machine without --wake, on a lane
with requests in flight, inside a quiet window, or when it would change a
locked load key. It holds the machine on while it runs, takes the lane's
tuning lock (other requests go elsewhere meanwhile), never interrupts a
request (Ctrl-C waits for the requests in flight to end), and lets both go
at the end. Between runs (never during one) a heartbeat renews the lock and
reads whether a quiet window began. A write whose apply the gateway held
back (`apply.pending`: the model was busy) is waited for, never taken as
applied; a model the gateway pauses (503 `model_paused`) is asked again
later.

Standard library only. The gateway client (key handling, errors) is
`ai_gateway.Gateway`.
"""

import contextlib
import hashlib
import json
import math
import os
import random
import signal
import statistics
import string
import sys
import threading
import time
import urllib.parse
import uuid
from concurrent.futures import ThreadPoolExecutor
from concurrent.futures import TimeoutError as FutureTimeout

OK, FAIL, USAGE = 0, 1, 2
INTERRUPTED = 130

# The noise is the standard deviation (SD) of one run, relative to the
# runs' mean, per metric: the sample SD of the noise runs, then pooled with
# each experiment's own runs. Sample SD, not MAD x 1.4826: with three to
# five runs the MAD is the middle gap of a handful of numbers (with three
# runs, the smaller of two), half the true spread on average and zero
# whenever two runs agree; there is no run to be robust against, since a
# run that fails is never a number.
NOISE_FLOOR = 0.02  # the SD is never taken under 2%: a few runs can look steadier than the machine
NOISE_RUNS = 3      # runs of the same settings that measure the noise before any lever
NOISE_MORE = 2      # more such runs when those 3 have an SD over the floor
# An SD over 20%: stop. A kept win must clear 1.7 SD (34% here) with room
# to spare, so only a speed-up of +40% or more could show, and runs that
# differ by 40% from one another say little about any setting. The first
# real box measured an SD of about 6 to 8%.
TOO_NOISY = 0.20
REPEAT_ABOVE = 0.05  # an SD over 5%: a win must hold over two candidate runs or more
SURPRISE = 0.20     # so must a win over 20%
# The keep rule. A trial's gain is measured against the mean of the
# controls; with n candidate runs and n + 1 controls its standard error is
# SD x sqrt(1/n + 1/(n+1)) (SD x sqrt(1.5) for one candidate between two
# controls). A win is kept when it clears WIN_SD x SD by CONFIDENCE
# standard errors, or (at the last run) when it is larger than both
# WIN_SD x SD and CONFIDENCE standard errors; rejected once CONFIDENCE
# standard errors above it stay under WIN_SD x SD. At an SD of 7% that keeps
# a real +20% in about 94 of 100 runs and a real +5% in about 3 (see the
# seeded simulation in cli/tests/sb_ai_optimize.py): a smaller WIN_SD keeps
# more small wins that are noise, fewer MAX_ROUNDS miss more real ones.
WIN_SD = 1.7
CONFIDENCE = 2.0
MAX_ROUNDS = 10     # candidate runs per change at most (one more control than that)
PLATEAU = 3         # three levers in a row inside the noise: stop
MIN_SAMPLES = 6     # decode samples per run (the median is used)
MAX_SAMPLES = 24    # the most samples per run the noise rule may raise to
HEARTBEAT_S = 30    # renew the lock and read the quiet window at most this often
APPLY_WAIT_S = 600  # how long a held-back apply (model busy) may stay pending
# The gateway's worst case for an unload, with its default engine settings:
# it waits `apply_wait_secs` (120 s) for the model's requests to end, its
# unload call to the engine up to 60 s, then up to `load_timeout_secs`
# (600 s) for the model's process to exit. A client waits longer than all
# of it, so an unload the gateway still finishes is never cut off.
GATEWAY_APPLY_WAIT_S = 120
GATEWAY_UNLOAD_CALL_S = 60
GATEWAY_LOAD_TIMEOUT_S = 600
UNLOAD_TIMEOUT = GATEWAY_APPLY_WAIT_S + GATEWAY_UNLOAD_CALL_S + GATEWAY_LOAD_TIMEOUT_S + 30


def lane_unload_timeout(engine):
    """How long a client waits for an unload on one lane: the lane's own
    `apply_wait_secs` and `load_timeout_secs` (the settings view names them
    under `load.engine`), the gateway's unload call and 30 s spare. A
    gateway that does not name them gets UNLOAD_TIMEOUT (the defaults')."""
    waits = [engine.get("apply_wait_secs"), engine.get("load_timeout_secs")]
    if not all(isinstance(wait, (int, float)) and not isinstance(wait, bool) and wait >= 0
               for wait in waits):
        return UNLOAD_TIMEOUT
    return waits[0] + GATEWAY_UNLOAD_CALL_S + waits[1] + 30


# A load-time run unloads and loads the model this many times.
LOAD_TIMES = 3
PAUSED_WAIT_S = 300  # how long a request retries a paused model (503 model_paused)

# The lever catalogue, per engine kind, ordered by expected payoff. Each
# entry: the load settings it changes, the metric it is judged on, the gate
# its candidate must pass, and what the skill's evidence says about it.
# `values` derive candidates from the current best (see `candidates`).
CATALOGUE = {
    "llama_cpp_router": [
        {"lever": "spec_type", "keys": ["spec_type"], "metric": "decode", "gate": "greedy",
         "note": "speculative decoding (MTP when the model has an MTP head or a draft file): "
                 "+22% to +76% measured on large GPUs; it can lose on bandwidth-poor chips "
                 "and on hard prompts"},
        {"lever": "draft_max", "keys": ["draft_max"], "metric": "weighted", "gate": "greedy",
         "kind": "sweep", "values": [1, 2, 3, 4], "needs_spec": True,
         "note": "the best draft length differs by concurrency and context; there is no "
                 "per-request draft length, so one value serves every workload"},
        {"lever": "flash_attn", "keys": ["flash_attn"], "metric": "decode", "gate": "greedy",
         "note": "faster attention kernels; usually a win on GPUs"},
        {"lever": "ubatch_size", "keys": ["batch_size", "ubatch_size"], "metric": "prefill",
         "gate": "greedy", "values": [512, 1024, 2048],
         "note": "bigger batches speed prefill and can stall decode for others"},
        {"lever": "parallel", "keys": ["parallel", "ctx_size"], "metric": "aggregate",
         "gate": "soak",
         "note": "more slots serve more users at once; context = slots x per-slot context, "
                 "checked for fit first and proven with a concurrent full-context soak"},
        {"lever": "cache_type", "keys": ["cache_type_k", "cache_type_v"], "metric": "decode",
         "gate": "needle", "values": ["f16", "q8_0"],
         "note": "q8_0 halves KV memory but halved decode on a model with KV in 16 of 64 "
                 "layers; a lossy change, so it must find needles across the context"},
        {"lever": "n_gpu_layers", "keys": ["n_gpu_layers", "fit"], "metric": "decode",
         "gate": "greedy",
         "note": "every layer on the GPU against the engine's own fit"},
        {"lever": "threads", "keys": ["threads"], "metric": "decode", "gate": "greedy",
         "explicit_only": True,
         "note": "CPU threads; matters on unified-memory machines (Apple)"},
        {"lever": "mmap", "keys": ["mmap"], "metric": "load_s", "gate": "none",
         "note": "memory-mapped loading against reading the file at once: changes load "
                 "time, not speed"},
        {"lever": "sleep_idle_seconds", "keys": ["sleep_idle_seconds"], "kind": "advice",
         "note": "how long an idle model stays loaded, weighed against measured swap cost"},
    ],
}

HIGHER_IS_BETTER = {"decode": True, "prefill": True, "aggregate": True, "weighted": True,
                    "ttft_ms": False, "load_s": False}

FILLER = ("river stone paper cloud engine garden silver market winter orange signal harbor "
          "violet candle forest mirror thunder copper meadow lantern").split()

CHAT_PROMPT = ("You are a careful assistant. Read the notes, then answer in one short "
               "paragraph.\n\nNotes:\n" + " ".join(FILLER * 12) +
               "\n\nQuestion: what do the notes list?")
READ_PROMPT = ("Read the document. Reply with one line about what it is about.\n\nDocument:\n"
               + " ".join(FILLER * 120))
GREEDY_PROMPT = "List the first ten prime numbers, separated by commas, and nothing else."


# Lane states of a machine that is up and serves requests.
READY_STATES = ("healthy", "idle_countdown")

class TuneError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


class Halt(Exception):
    """Stop at a safe point (no request in flight): a quiet window began
    (`code` OK) or the tuning lock is gone (`code` FAIL)."""

    def __init__(self, reason, code):
        super().__init__(reason)
        self.reason = self.message = reason
        self.code = code


class Interrupted(Exception):
    pass


class ApplyFailed(Exception):
    """A trial's settings did not apply or load: the trial failed, nothing
    was measured, so it says nothing about the lever."""


@contextlib.contextmanager
def stop_signals_deferred():
    """Hold back Ctrl-C, SIGTERM and SIGHUP while a hold or a lock is taken
    and its id recorded: a stop that arrives meanwhile is only noted, and
    replayed to the handler it was meant for once the id is known, so the
    release can let it go. Main thread only (where Python runs handlers)."""
    if threading.current_thread() is not threading.main_thread():
        yield
        return
    pending = []
    saved = {}

    def note(signum, _frame):
        pending.append(signum)

    for name in ("SIGINT", "SIGTERM", "SIGHUP"):
        signum = getattr(signal, name, None)
        if signum is not None:
            saved[signum] = signal.signal(signum, note)
    try:
        yield
    finally:
        for signum, handler in saved.items():
            signal.signal(signum, handler)
        if pending:
            signum = pending[0]
            handler = saved[signum]
            if callable(handler):
                handler(signum, None)
            elif handler == signal.SIG_DFL:
                os.kill(os.getpid(), signum)


def parse_budget(text):
    """30m, 2h, 1h30m, 90s -> seconds."""
    total, number = 0, ""
    for ch in (text or "").strip().lower():
        if ch.isdigit():
            number += ch
        elif ch in "hms" and number:
            total += int(number) * {"h": 3600, "m": 60, "s": 1}[ch]
            number = ""
        else:
            raise TuneError(USAGE, f"--budget {text}: use 30m, 2h, 1h30m or 90s")
    if number:
        total += int(number) * 60
    if total <= 0:
        raise TuneError(USAGE, f"--budget {text}: use 30m, 2h, 1h30m or 90s")
    return total


def parse_concurrency(text):
    try:
        values = sorted({int(part) for part in (text or "1").split(",") if part.strip()})
    except ValueError:
        raise TuneError(USAGE, f"--concurrency {text}: use numbers like 1,2,3") from None
    if not values or values[0] < 1 or values[-1] > 64:
        raise TuneError(USAGE, f"--concurrency {text}: use numbers from 1 to 64")
    return values


def looping(text):
    """A looping answer: some six-word run repeats five times or more."""
    words = text.split()
    if len(words) < 30:
        return False
    counts = {}
    for i in range(len(words) - 5):
        key = " ".join(words[i:i + 6])
        counts[key] = counts.get(key, 0) + 1
        if counts[key] >= 5:
            return True
    return False


def median(values):
    return statistics.median(values) if values else None


def short_hash(text):
    return hashlib.sha256(text.encode()).hexdigest()[:12]


def noise_sds(runs):
    """Per metric, the noise of runs of the same settings: their sample
    standard deviation over their mean (0.07 = 7%). A metric some run
    lacks is left out; fewer than two runs measure nothing."""
    out = {}
    if len(runs) < 2:
        return out
    for key in HIGHER_IS_BETTER:
        values = [run[key] for run in runs if isinstance(run.get(key), (int, float))]
        if len(values) != len(runs):
            continue
        mean = statistics.fmean(values)
        if mean > 0:
            out[key] = statistics.stdev(values) / mean
    return out


def pooled_sd(groups):
    """The relative SD pooled over groups of runs that each share their
    own settings (each group around its own mean), and its degrees of
    freedom: (sd, df), (None, 0) when no group has two runs."""
    squares, df = 0.0, 0
    for values in groups:
        if len(values) < 2:
            continue
        mean = statistics.fmean(values)
        if mean <= 0:
            continue
        squares += sum(((value - mean) / mean) ** 2 for value in values)
        df += len(values) - 1
    if df == 0:
        return None, 0
    return math.sqrt(squares / df), df

def run_kind(metrics):
    """The key a run's cost is kept under: the metrics it measured."""
    return "+".join(sorted(set(metrics)))


class Tuner:
    def __init__(self, gw, args, fail_types, out=sys.stdout, err=sys.stderr):
        self.gw = gw
        self.args = args
        self.fail_types = fail_types
        self.out = out
        self.err = err
        self.run_id = "tune_" + uuid.uuid4().hex[:10]
        self.samples = max(MIN_SAMPLES, int(getattr(args, "samples", None) or MIN_SAMPLES))
        self.requested_samples = self.samples
        # Measured before the first lever (`measure_noise`): per metric, the
        # SD of one run of the same settings, and those runs themselves.
        self.noise = {}
        self.noise_data = []
        self.noise_runs = 0
        self.noise_metrics = ("decode",)
        self.sample_counts = []
        self.failed = []
        self.concurrency = parse_concurrency(args.concurrency)
        self.budget = parse_budget(args.budget)
        self.workload = args.workload
        # The budget's clock (a test passes its own).
        self.clock = time.monotonic
        self.started = self.clock()
        # Seconds each kind of run and each apply took: what a next one costs.
        self.run_secs = {}
        self.apply_secs = []
        # Seconds each kind of gate took (a change's first trial runs one),
        # and each unload and load of a load-time run.
        self.gate_secs = {}
        self.load_secs = []
        self.budget_cut = False
        self.untuned_reason = None
        self.lock_id = None
        self.hold = None
        self.stopping = False
        self.rows = []
        self.notes = []
        self.unverified = []
        self.gains = []
        self.best_metrics = None
        self.applied_doc = None
        self.revision = 0
        self.base_revision = 0
        self.rng = random.Random()
        self.baseline = {}
        self.sweep_tables = {}
        self.ceiling_row = {}
        self.greedy_reference = None
        self.last_control = None
        self.current_lever, self.current_metric = "baseline", "decode"
        self.last_beat = 0.0
        poll = getattr(args, "poll_secs", None)
        # Test runs pass a short poll; every wait scales with it.
        self.scale = 1.0 if poll is None else max(0.001, float(poll) / 2.0)

    # ------------------------------------------------------------ talking

    def log(self, message):
        if not self.args.json:
            print(message, file=self.out, flush=True)

    def say(self, message):
        print(f"sb ai tune: {message}", file=self.err, flush=True)

    def call(self, method, path, body=None, timeout=120, headers=None, doing="talking to the gateway"):
        try:
            return self.gw.call(method, path, body=body, timeout=timeout, headers=headers or {})
        except self.fail_types as error:
            raise TuneError(FAIL, f"{doing}: {getattr(error, 'message', error)}") from None

    # ------------------------------------------------------------ the target

    def resolve(self):
        path = f"/v1/models/{urllib.parse.quote(self.args.model, safe='/')}/settings"
        view = self.call("GET", path, doing="reading the settings")
        targets = view.get("targets") or []
        # A public name can share its id with one of its targets: that exact target is meant.
        exact = [t for t in targets if t.get("target") == self.args.model]
        if len(targets) > 1 and len(exact) == 1:
            targets = exact
        if len(targets) != 1:
            names = ", ".join(t.get("target", "?") for t in targets)
            raise TuneError(USAGE, f"{self.args.model} reaches {len(targets)} targets ({names}); name one")
        target = targets[0]
        if "document" not in target:
            raise TuneError(FAIL, "tuning needs an operator or admin key (this key cannot change settings)")
        self.target = target["target"]
        self.provider, _, self.model = self.target.partition("/")
        self.doc = target.get("document") or {}
        self.revision = int(target.get("revision") or 0)
        self.base_revision = self.revision
        self.best = json.loads(json.dumps(self.doc))
        self.applied_doc = json.loads(json.dumps(self.doc))
        load_view = target.get("load") or {}
        engine = load_view.get("engine") or {}
        self.lane = engine.get("lane") or self.provider
        self.unload_timeout = lane_unload_timeout(engine)
        self.kind = engine.get("kind") or "llama_cpp_router"
        if self.kind not in CATALOGUE:
            raise TuneError(FAIL, f"no lever catalogue for engine kind {self.kind}")
        self.locked = {key[5:] for key in self.doc.get("locked") or [] if key.startswith("load.")}

    def engine_identity(self):
        """The engine build and the model file's identity (a hash, never a path)."""
        build, model_file, row = "unknown", "unknown", {}
        try:
            engines = self.call("GET", "/v1/engines", doing="reading the engines")
        except TuneError:
            return build, model_file, row
        for lane in engines.get("data") or []:
            if lane.get("lane") != self.lane:
                continue
            build = str(lane.get("build") or "unknown")
            for model in lane.get("models") or []:
                if model.get("id") == self.model:
                    row = model
                    model_file = str(model.get("file") or short_hash(json.dumps(model.get("args"))))
        return build, model_file, row

    def lane_state(self):
        capacity = self.call("GET", "/v1/workloads/capacity", doing="reading the lanes")
        for lane in capacity.get("data") or []:
            if lane.get("name") == self.lane:
                return lane
        return None

    # ------------------------------------------------------------ the plan

    def levers(self):
        catalogue = CATALOGUE[self.kind]
        wanted = None
        if self.args.levers:
            wanted = [name.strip() for name in self.args.levers.split(",") if name.strip()]
            known = {entry["lever"] for entry in catalogue}
            unknown = [name for name in wanted if name not in known]
            if unknown:
                raise TuneError(USAGE, f"unknown lever {', '.join(unknown)}; levers: {', '.join(sorted(known))}")
        plan = []
        for entry in catalogue:
            if wanted is not None and entry["lever"] not in wanted:
                continue
            if wanted is None and entry.get("explicit_only"):
                continue
            touched = sorted(set(entry["keys"]) & self.locked)
            if touched:
                if wanted is not None:
                    raise TuneError(FAIL, f"{self.target} locks load.{', load.'.join(touched)}; "
                                          f"the {entry['lever']} lever would change it")
                self.notes.append(f"{entry['lever']}: skipped, load.{touched[0]} is locked")
                continue
            plan.append(entry)
        return plan

    def candidates(self, entry, load, listing_row):
        """Candidate values for a lever, from the current best load settings.
        Returns (list of {key: value} changes, why-none)."""
        lever = entry["lever"]
        if lever == "spec_type":
            current = self.spec_type_now(load, listing_row)
            if current and current != "none":
                return [{"spec_type": "none"}], None
            spec = listing_row.get("speculative") or {}
            # Real routers list `sidecars` only for cached Hugging Face
            # models, never for preset ones: the preset/overlay keys and a
            # loaded instance's arguments (`speculative`) come first.
            sidecars = " ".join(str(s) for s in listing_row.get("sidecars") or []).lower()
            if spec.get("mtp") or "mtp" in sidecars:
                return [{"spec_type": "draft-mtp"}], None
            if load.get("spec_draft_model") or spec.get("draft_model") or "draft" in sidecars:
                return [{"spec_type": "draft-simple"}], None
            return [], "no MTP head or draft model file is known for this model"
        if lever == "draft_max":
            current = self.spec_type_now(load, listing_row)
            if not current or current == "none":
                return [], "speculative decoding is off"
            return [{"draft_max": value} for value in entry["values"]], None
        if lever == "flash_attn":
            return [{"flash_attn": "off" if load.get("flash_attn") == "on" else "on"}], None
        if lever == "ubatch_size":
            current = int(load.get("ubatch_size") or 512)
            batch = int(load.get("batch_size") or 2048)
            return [{"ubatch_size": v, "batch_size": max(batch, v)}
                    for v in entry["values"] if v != current], None
        if lever == "parallel":
            slots = int(load.get("parallel") or 1)
            ctx = int(load.get("ctx_size") or listing_row.get("n_ctx") or 4096)
            per_slot = max(512, ctx // slots)
            top = max(self.concurrency)
            if top <= 1:
                return [], "--concurrency is 1, so more slots cannot help"
            out = []
            estimate = (self.best.get("request_load") or {}).get("estimate")
            for n in range(1, top + 1):
                if n == slots:
                    continue
                change = {"parallel": n, "ctx_size": n * per_slot}
                if estimate and not self.fits(estimate, n * per_slot, load):
                    self.notes.append(f"parallel {n}: would not fit ({n * per_slot} tokens of context)")
                    continue
                out.append(change)
            if not estimate:
                self.unverified.append("parallel: no memory estimate in the settings "
                                       "(request_load.estimate), so the engine's load decided the fit")
            return out, None
        if lever == "cache_type":
            quant = load.get("cache_type_k") == "q8_0" and load.get("cache_type_v") == "q8_0"
            value = "f16" if quant else "q8_0"
            return [{"cache_type_k": value, "cache_type_v": value}], None
        if lever == "n_gpu_layers":
            if load.get("n_gpu_layers") == "all":
                return [{"n_gpu_layers": None, "fit": "on"}], None
            return [{"n_gpu_layers": "all", "fit": None}], None
        if lever == "threads":
            current = int(load.get("threads") or 8)
            return [{"threads": v} for v in sorted({max(1, current // 2), current * 2}) if v != current], None
        if lever == "mmap":
            return [{"mmap": load.get("mmap") is False}], None
        return [], "advice only"

    @staticmethod
    def spec_type_now(load, listing_row):
        """The speculative type in force: our settings, else what the preset
        or the loaded instance says (the gateway's `speculative` view)."""
        if "spec_type" in load:
            return load.get("spec_type")
        return (listing_row.get("speculative") or {}).get("spec_type")

    @staticmethod
    def fits(estimate, ctx, load):
        sizes = {"f32": 4.0, "f16": 2.0, "bf16": 2.0, "q8_0": 34 / 32, "q4_0": 18 / 32}
        k = sizes.get(load.get("cache_type_k") or "f16", 2.0)
        v = sizes.get(load.get("cache_type_v") or "f16", 2.0)
        head = int(estimate.get("head_dim") or 128)
        value = int(estimate.get("value_dim") or head)
        kv = int(estimate.get("kv_layers") or 0) * ctx * int(estimate.get("kv_heads") or 0) * (head * k + value * v)
        return int(estimate.get("weights_bytes") or 0) + kv <= int(estimate.get("budget_bytes") or 0)

    # ------------------------------------------------------------ settings

    def put_doc(self, doc, note):
        """Write `doc` as a new revision and apply it now. The revision we
        edit rides `If-Match` (a quoted ETag). An apply the gateway held back
        (`pending`: the model still served requests) is waited for; one
        that does not happen in time stops the run."""
        path = f"/v1/models/{urllib.parse.quote(self.target, safe='/')}/settings"
        body = dict(doc)
        body.update({"apply": "now", "note": note})
        # From here until the gateway answers, what the engine runs is
        # unknown: a Ctrl-C or a client timeout can stop us while the
        # gateway still applies `doc`. Unknown makes `restore_best` write.
        self.applied_doc = None
        started = self.clock()
        try:
            answer = self.gw.call("PUT", path, body=body, timeout=900,
                                  headers={"If-Match": f'"{self.revision}"'})
        except self.fail_types as error:
            if getattr(error, "status", None) != 409 or getattr(error, "reason", "") == "model_busy":
                raise TuneError(FAIL, f"applying settings: {getattr(error, 'message', error)}") from None
            # Our own earlier write landed after all (an interrupted call):
            # take the current revision and write again.
            view = self.call("GET", path, doing="reading settings")
            self.revision = int((view.get("targets") or [{}])[0].get("revision") or 0)
            answer = self.call("PUT", path, body=body, timeout=900, doing="applying settings",
                               headers={"If-Match": f'"{self.revision}"'})
        self.revision = int(answer.get("revision") or self.revision + 1)
        apply = answer.get("apply") or {}
        self.applied_doc = json.loads(json.dumps(doc))
        if apply and apply.get("ok") is False:
            raise TuneError(FAIL, apply.get("message") or apply.get("error") or "the engine did not load it")
        if apply.get("pending"):
            self.wait_applied(apply)
        self.apply_secs.append(self.clock() - started)
        return answer

    def wait_applied(self, apply):
        """A held-back apply (`pending: true`, e.g. `reason: model_busy`):
        the engine re-reads the settings once the model is idle. Wait until
        the lane has nothing pending for this model; never measure before.
        Still pending at the bound: the run stops (never a rejection)."""
        self.log(f"  settings revision {self.revision} written; the engine applies it when "
                 f"{self.model} is idle ({apply.get('reason') or 'pending'}), waiting")
        deadline = time.monotonic() + APPLY_WAIT_S * self.scale
        poll = max(0.05, 2.0 * self.scale)
        while time.monotonic() < deadline:
            lane = self.engine_lane()
            if lane is not None and not lane.get("pending_apply") \
                    and self.model not in (lane.get("deferred") or []):
                return
            time.sleep(poll)
        raise Halt(f"settings revision {self.revision} of {self.target} is still not applied "
                   f"after {int(APPLY_WAIT_S * self.scale)} s (the model stays busy); stopped", FAIL)

    def engine_lane(self):
        try:
            engines = self.call("GET", "/v1/engines", doing="reading engines")
        except TuneError:
            return None
        for lane in engines.get("data") or []:
            if lane.get("lane") == self.lane:
                return lane
        return None

    def heartbeat(self, force=False):
        """At a safe point (no request in flight): renew the tuning lock and
        read the quiet window. A window that began, or a lock the gateway no
        longer has (it restarted, or the lock ran out), stops the run."""
        if not self.lock_id:
            return
        now = time.monotonic()
        if not force and now - self.last_beat < HEARTBEAT_S * self.scale:
            return
        self.last_beat = now
        ttl = int(max(60, self.budget - self.elapsed() + 900))
        try:
            beat = self.gw.call("PATCH", f"/v1/optimize/locks/{self.lock_id}",
                                body={"ttl_secs": min(ttl, 14400)}, timeout=30, headers={})
        except self.fail_types as error:
            if getattr(error, "status", None) == 404:
                self.lock_id = None
                raise Halt("the tuning lock is gone (it ran out or the gateway restarted); "
                           "stopped at a safe point", FAIL) from None
            self.say(f"renewing the tuning lock: {getattr(error, 'message', error)}")
            return
        window = beat.get("quiet_window")
        if window:
            raise Halt(f"the quiet window {window} began; stopped at a safe point", OK)

    def with_change(self, doc, change):
        new = json.loads(json.dumps(doc))
        load = new.setdefault("load", {})
        for key, value in change.items():
            if value is None:
                load.pop(key, None)
            else:
                load[key] = value
        if not load:
            new.pop("load", None)
        return new

    # ------------------------------------------------------------ requests

    def run_parallel(self, jobs):
        """Run callables on worker threads and wait for all. Ctrl-C never
        interrupts a request: it waits for the ones in flight, then stops."""
        if not jobs:
            return []
        pool = ThreadPoolExecutor(max_workers=len(jobs))
        futures = [pool.submit(job) for job in jobs]
        results = []
        interrupted = False
        for future in futures:
            while True:
                try:
                    results.append(future.result(timeout=0.2))
                    break
                except KeyboardInterrupt:
                    interrupted = True
                    self.stopping = True
                except (FutureTimeout, TimeoutError):
                    continue
                except Exception as error:  # noqa: BLE001 - a failed request is a result
                    results.append({"error": str(getattr(error, "message", error))})
                    break
        pool.shutdown(wait=True)
        if interrupted:
            raise Interrupted()
        return results

    def chat(self, prompt, max_tokens, temperature=None, fresh=True):
        if fresh:
            prompt = f"Run {uuid.uuid4().hex[:12]}.\n" + prompt
        body = {"model": self.target, "messages": [{"role": "user", "content": prompt}],
                "max_tokens": max_tokens, "stream": True, "stream_options": {"include_usage": True},
                "reasoning_effort": "none"}
        if temperature is not None:
            body["temperature"] = temperature
            body["seed"] = 1
        headers = {"x-switchback-tune-lock": self.lock_id} if self.lock_id else {}
        deadline = time.monotonic() + PAUSED_WAIT_S * self.scale
        while True:
            try:
                return self.chat_once(body, headers)
            except self.fail_types as error:
                paused = getattr(error, "status", None) == 503 and "model_paused" in (
                    getattr(error, "reason", ""), getattr(error, "error_type", ""))
                if not paused or time.monotonic() >= deadline or self.stopping:
                    raise
                # The gateway closed the model's gate (an apply or an unload
                # runs): ask again later; never a failed answer.
                time.sleep(max(0.05, 5.0 * self.scale))

    def chat_once(self, body, headers):
        started = time.monotonic()
        first, usage, text = None, None, []
        for when, chunk in self.gw.stream("/v1/chat/completions", body, timeout=900, headers=headers):
            if isinstance(chunk.get("usage"), dict) and chunk["usage"]:
                usage = chunk["usage"]
            for choice in chunk.get("choices") or []:
                delta = choice.get("delta") or {}
                piece = delta.get("content") or ""
                if piece or delta.get("reasoning_content"):
                    if first is None:
                        first = when
                text.append(piece)
        ended = time.monotonic()
        if first is None or not usage:
            return {"error": "the answer carried no token or no usage"}
        ttft = (first - started) * 1000
        prompt_tokens = int(usage.get("prompt_tokens") or 0)
        completion = int(usage.get("completion_tokens") or 0)
        decoded = ended - first
        return {"ttft_ms": ttft, "prompt_tps": prompt_tokens / (ttft / 1000) if ttft > 0 else 0.0,
                "decode_tps": completion / decoded if decoded > 0 else 0.0,
                "completion_tokens": completion, "started": started, "ended": ended,
                "text": "".join(text)}

    def check_answers(self, answers):
        failed = [a["error"] for a in answers if "error" in a]
        if failed:
            return f"{len(failed)} request(s) failed ({failed[0]})"
        empty = [a for a in answers if not a.get("text", "").strip()]
        if empty:
            return f"{len(empty)} empty answer(s)"
        if any(looping(a.get("text", "")) for a in answers):
            return "a looping answer (the same words over and over)"
        return None

    def measure(self, *metrics):
        """One run: the metrics asked for plus decode and time to first
        token, each the median over the run's samples. Fresh prompts (no
        cache hits). The first request after a load is discarded. What the
        run took is kept per kind of run (`run_cost`)."""
        if self.stopping:
            raise Interrupted()
        self.heartbeat()
        started = self.clock()
        self.run_parallel([lambda: self.chat(CHAT_PROMPT, 64)])  # warm-up, discarded
        out = {}
        answers = []
        for _ in range(self.samples):
            answers.extend(self.run_parallel([lambda: self.chat(CHAT_PROMPT, 128)]))
        problem = self.check_answers(answers)
        if problem:
            return None, problem
        out["decode"] = median([a["decode_tps"] for a in answers])
        out["ttft_ms"] = median([a["ttft_ms"] for a in answers])
        if "prefill" in metrics or self.workload in ("read", "mixed"):
            reads = []
            for _ in range(self.samples):
                reads.extend(self.run_parallel([lambda: self.chat(READ_PROMPT, 16)]))
            problem = self.check_answers(reads)
            if problem:
                return None, problem
            out["prefill"] = median([a["prompt_tps"] for a in reads])
        if "aggregate" in metrics or "weighted" in metrics:
            per = {}
            for n in self.concurrency:
                values = []
                for _ in range(max(2, self.samples // 3)):
                    batch = self.run_parallel([lambda: self.chat(CHAT_PROMPT, 128)] * n)
                    problem = self.check_answers(batch)
                    if problem:
                        return None, problem
                    span = max(a["ended"] for a in batch) - min(a["started"] for a in batch)
                    values.append(sum(a["completion_tokens"] for a in batch) / span if span > 0 else 0)
                per[n] = median(values)
            out["per_concurrency"] = per
            out["aggregate"] = per[max(self.concurrency)]
            out["weighted"] = sum(per.values()) / len(per)
        if "load_s" in metrics:
            out["load_s"] = self.load_time()
        self.run_secs.setdefault(run_kind(metrics), []).append(self.clock() - started)
        return out, None

    def run_cost(self, metric):
        """Seconds a run measuring `metric` takes: the mean of such runs so
        far, else of the noise runs (they measure every metric the plan
        uses, so they cost at least as much), else 0. A load-time run not
        measured yet is not like a noise run: it unloads and loads the model
        LOAD_TIMES times, so it costs at least that many applies (each a
        reload)."""
        times = self.run_secs.get(run_kind((metric,)))
        if times:
            return statistics.fmean(times)
        noise_run = statistics.fmean(self.run_secs.get(run_kind(self.noise_metrics)) or [0.0])
        if metric == "load_s":
            return max(noise_run, LOAD_TIMES * self.apply_cost())
        return noise_run

    def gate_cost(self, kind):
        """Seconds a gate of `kind` takes: the mean of such gates so far
        (the greedy reference counts for the greedy gate), else one noise
        run; nothing for a lever with no gate."""
        if kind in (None, "none"):
            return 0.0
        times = self.gate_secs.get(kind)
        if times:
            return statistics.fmean(times)
        return statistics.fmean(self.run_secs.get(run_kind(self.noise_metrics)) or [0.0])

    def apply_cost(self):
        """Seconds a settings write with its apply (a reload) takes; before
        any apply, an unload and load of a load-time run (also a reload)."""
        times = self.apply_secs or self.load_secs
        return statistics.fmean(times) if times else 0.0

    def round_cost(self, metric):
        """One more round of an experiment: apply the candidate, measure
        it, put the best back, measure a control."""
        return 2 * (self.apply_cost() + self.run_cost(metric))

    def budget_fits(self, seconds):
        return self.elapsed() + seconds <= self.budget

    def load_time(self):
        path = f"/v1/models/{urllib.parse.quote(self.target, safe='/')}"
        times = []
        for _ in range(LOAD_TIMES):
            reload_started = self.clock()
            self.call("POST", path + "/unload", body={}, timeout=self.unload_timeout, doing="unloading")
            started = time.monotonic()
            self.call("POST", path + "/load", body={"wake": False}, timeout=900, doing="loading")
            times.append(time.monotonic() - started)
            self.load_secs.append(self.clock() - reload_started)
        return median(times)

    # ------------------------------------------------------------ gates

    def gate(self, kind, candidate_doc):
        """None when the candidate passes its gate, else why not."""
        if kind == "greedy":
            reference = self.greedy_reference
            answer = self.run_parallel([lambda: self.chat(GREEDY_PROMPT, 64, temperature=0, fresh=False)])[0]
            if "error" in answer:
                return f"greedy check failed: {answer['error']}"
            if reference is not None and answer["text"].strip() != reference.strip():
                return ("greedy output changed at temperature 0 (a change claimed lossless must "
                        "give the same answer; small numeric differences can cause this)")
            return None
        if kind == "needle":
            ctx = int((candidate_doc.get("load") or {}).get("ctx_size") or 8192)
            return self.needles(ctx, 1)
        if kind == "soak":
            load = candidate_doc.get("load") or {}
            slots = int(load.get("parallel") or 1)
            ctx = int(load.get("ctx_size") or 4096)
            return self.needles(max(512, ctx // slots), slots)
        return None

    def needles(self, ctx_tokens, streams):
        """Needles at about 50% and 100% of `ctx_tokens`, on `streams`
        concurrent requests; every code must come back exactly."""
        def one():
            codes = ["".join(self.rng.choice(string.ascii_uppercase + string.digits) for _ in range(8))
                     for _ in range(2)]
            words = int(ctx_tokens * 0.85 / 1.3)
            filler = [FILLER[i % len(FILLER)] for i in range(words)]
            filler.insert(len(filler) // 2, f"NEEDLE-ONE is {codes[0]}.")
            filler.append(f"NEEDLE-TWO is {codes[1]}.")
            prompt = ("Find NEEDLE-ONE and NEEDLE-TWO in the document and reply with both codes.\n\n"
                      + " ".join(filler))
            answer = self.chat(prompt, 48, temperature=0, fresh=False)
            if "error" in answer:
                return f"a long-context request failed ({answer['error']})"
            missing = [code for code in codes if code not in answer.get("text", "")]
            if missing:
                return f"needle gate: {len(missing)} of 2 needles not found at {ctx_tokens} tokens"
            return None
        results = self.run_parallel([one] * streams)
        problems = [r for r in results if r]
        if problems:
            if streams > 1:
                return f"concurrent soak ({streams} x {ctx_tokens} tokens): {problems[0]}"
            return problems[0]
        return None

    # ------------------------------------------------------------ the ledger

    def history(self):
        path = "/v1/optimize/experiments?model=" + urllib.parse.quote(self.target, safe="")
        try:
            return self.call("GET", path, doing="reading the experiment ledger").get("data") or []
        except TuneError:
            return []

    def record(self, row):
        row = dict(row, target=self.target, run_id=self.run_id, model=self.model,
                   engine_build=self.build, model_file=self.model_file,
                   settings_revision=self.revision)
        self.rows.append(row)
        try:
            self.call("POST", "/v1/optimize/experiments", body=row, doing="writing the ledger")
        except TuneError as error:
            self.notes.append(f"ledger write failed: {error.message}")

    def rejected_before(self, past, lever, change):
        """The earlier rejection of this exact change, when nothing changed since."""
        value = json.dumps(change, sort_keys=True)
        for row in past:
            if row.get("lever") != lever or row.get("role") != "verdict":
                continue
            if json.dumps(row.get("value"), sort_keys=True) != value:
                continue
            if row.get("verdict") != "rejected":
                return None  # its latest verdict was not a rejection
            if row.get("engine_build") != self.build:
                self.notes.append(f"{lever} {value}: testing again, the engine build changed "
                                  f"({row.get('engine_build')} -> {self.build})")
                return None
            if row.get("model_file") != self.model_file:
                self.notes.append(f"{lever} {value}: testing again, the model file changed")
                return None
            return row
        return None

    # ------------------------------------------------------------ experiments

    def elapsed(self):
        return self.clock() - self.started

    def out_of_budget(self):
        return self.elapsed() >= self.budget

    def sd_for(self, metric):
        """The noise the noise runs measured on `metric` (its SD of one run),
        never under the floor. A metric they did not measure (load time)
        takes the widest one measured."""
        noise = getattr(self, "noise", {})
        sd = noise.get(metric)
        if sd is None:
            sd = max(noise.values(), default=0.0)
        return max(NOISE_FLOOR, sd)

    def noise_sd(self, metric):
        """The SD the judge starts from before an experiment adds runs of
        its own: `experiment_sd` over the noise runs alone (floored and
        widened alike). The printed bar, the `--json` bar, the win-twice
        rule and the budget's round count all use it."""
        return self.experiment_sd(metric, [], [])

    def experiment_sd(self, metric, controls, cands):
        """The SD of one run on `metric` for an experiment: pooled over the
        noise runs (around their mean), the experiment's controls (around
        theirs) and its candidate runs (around theirs), never under the
        floor, and widened by 1 + 1/sqrt(2 df) for what so few runs cannot
        pin down (an SD from 5 runs is off by a third as often as not)."""
        def values(runs):
            return [run[metric] for run in runs if isinstance(run.get(metric), (int, float))]
        noise = [run for run in getattr(self, "noise_data", [])
                 if not any(run is control for control in controls)]
        sd, df = pooled_sd([values(noise), values(controls), values(cands)])
        if sd is None:
            sd, df = self.sd_for(metric), max(1, getattr(self, "noise_runs", 0) - 1)
        return max(NOISE_FLOOR, sd) * (1 + 1 / math.sqrt(2 * df))

    def judge(self, metric, controls, cands, last):
        """(decision, gain, reason, bar) for a change after its latest
        candidate run: `decision` True (keep), False (reject) or None (not
        clear yet: one more candidate run and control). `last`: no run
        follows, so decide now. See WIN_SD and CONFIDENCE."""
        higher = HIGHER_IS_BETTER[metric]
        sd = self.experiment_sd(metric, controls, cands)
        base = statistics.fmean(run[metric] for run in controls)
        now = statistics.fmean(run[metric] for run in cands)
        gain = (now / base - 1) if higher else (base / now - 1 if now > 0 else 0.0)
        n = len(cands)
        error = sd * math.sqrt(1 / n + 1 / len(controls))
        bar = WIN_SD * sd
        need = 2 if sd > REPEAT_ABOVE or gain > SURPRISE else 1
        said = (f"{metric} {gain * 100:+.1f}% over the controls after {n} run{'s' if n > 1 else ''} "
                f"(noise ±{sd * 100:.1f}%; a win must clear +{bar * 100:.1f}%)")
        if last:
            if gain <= max(CONFIDENCE * error, bar) or n < need:
                return False, gain, f"{said}: not clearly over the noise", bar
        elif gain + CONFIDENCE * error < bar:
            return False, gain, f"{said}: inside the noise", bar
        elif gain - CONFIDENCE * error <= bar or n < need:
            return None, gain, said, bar
        worse = self.side_loss(metric, controls, cands)
        if worse:
            return False, gain, f"{metric} gained {gain * 100:.0f}% but {worse}", bar
        return True, gain, said, bar

    def side_loss(self, metric, controls, cands):
        """Why another measured metric got worse than the controls' mean by
        more than a win on it would have to be (its own pooled noise), or
        None."""
        for other in ("decode", "ttft_ms", "prefill", "aggregate"):
            if other == metric or any(other not in run for run in controls + cands):
                continue
            base = statistics.fmean(run[other] for run in controls)
            now = statistics.fmean(run[other] for run in cands)
            if base <= 0:
                continue
            worse = (base - now) / base if HIGHER_IS_BETTER[other] else (now - base) / base
            sd = self.experiment_sd(other, controls, cands)
            allowed = max(CONFIDENCE * sd * math.sqrt(1 / len(cands) + 1 / len(controls)), WIN_SD * sd)
            if worse > allowed:
                return (f"{other} got {worse * 100:.0f}% worse than the controls' mean "
                        f"(more than the {allowed * 100:.0f}% its own noise allows)")
        return None

    def control(self):
        if self.last_control is not None and self.current_metric in self.last_control:
            return self.last_control
        metrics, problem = self.measure(self.current_metric)
        if problem:
            raise TuneError(FAIL, f"the control run failed: {problem}")
        self.record({"role": "control", "lever": self.current_lever, "value": None,
                     "metrics": metrics, "samples": self.samples})
        self.last_control = metrics
        return metrics

    def trial(self, entry, change, gate=True):
        """Apply the candidate, gate it (first run only) and measure it.
        (metrics, problem). Settings that do not apply or load raise
        `ApplyFailed`."""
        doc = self.with_change(self.best, change)
        try:
            self.put_doc(doc, f"tune {self.run_id} trial {entry['lever']}")
        except TuneError as error:
            raise ApplyFailed(error.message) from None
        if gate:
            started = self.clock()
            problem = self.gate(entry["gate"], doc)
            self.gate_secs.setdefault(entry["gate"], []).append(self.clock() - started)
            if problem:
                return None, problem
        metrics, problem = self.measure(entry["metric"])
        return metrics, problem

    def restore_best(self, why):
        """Put the best known settings back. Writes whenever what the engine
        runs differs from them or is unknown (`applied_doc` is None while a
        write is in flight or after one was cut off)."""
        if self.applied_doc is None or \
                json.dumps(self.applied_doc, sort_keys=True) != json.dumps(self.best, sort_keys=True):
            self.put_doc(self.best, f"tune {self.run_id} {why}")

    def restore_best_finally(self, why):
        """`restore_best` on the way out. A Ctrl-C during it does not leave
        a trial's settings in place: the restore runs once more; a second
        Ctrl-C there gives up and says how to undo."""
        for attempt in range(2):
            try:
                self.restore_best(why)
                return
            except KeyboardInterrupt:
                if attempt == 0:
                    self.say("Ctrl-C while restoring the best settings: restoring them again "
                             "(Ctrl-C once more to leave them as they are)")
        raise TuneError(FAIL, "interrupted twice while restoring; run `sb ai revert "
                              f"{self.target}` to undo the tuning writes")

    def experiment(self, entry, change):
        """Control, candidate, control, candidate, control... until `judge`
        decides (MAX_ROUNDS candidate runs at most). Returns (accepted,
        gain, reason); `accepted` None when a trial failed (its settings did
        not apply or load), so nothing was measured. A round the budget
        cannot pay for ends the experiment as rejected (nothing changes)."""
        lever, metric = entry["lever"], entry["metric"]
        self.current_lever, self.current_metric = lever, metric
        controls, cands = [self.control()], []
        first = None
        while True:
            try:
                cand, problem = self.trial(entry, change, gate=not cands)
            except ApplyFailed as failure:
                self.restore_best("control")
                self.last_control = None
                reason = f"did not load: {failure}"
                self.record({"role": "verdict", "lever": lever, "value": change, "metrics": None,
                             "samples": self.samples, "verdict": "failed", "reason": reason})
                return None, 0.0, reason
            self.restore_best("control")
            self.last_control = None
            if problem:
                self.record({"role": "verdict", "lever": lever, "value": change, "metrics": cand,
                             "samples": self.samples, "verdict": "rejected", "reason": problem})
                return False, 0.0, problem
            self.record({"role": "candidate", "lever": lever, "value": change, "metrics": cand,
                         "samples": self.samples, "round": len(cands) + 1})
            cands.append(cand)
            controls.append(self.control())
            accepted, gain, reason, bar = self.judge(metric, controls, cands,
                                                     last=len(cands) >= MAX_ROUNDS)
            if first is None:
                first = (gain, bar)
            if accepted is None:
                if self.stopping:
                    raise Interrupted()
                if self.budget_fits(self.round_cost(metric)):
                    if len(cands) == 1:
                        self.log(f"  {lever} {json.dumps(change)}: {reason}; not clear yet, "
                                 "measuring again")
                    continue
                self.budget_cut = True
                accepted, reason = False, f"{reason}: the budget ran out before it was clear"
            if not accepted and len(cands) > 1 and first[0] > first[1]:
                reason = f"a {first[0] * 100:+.0f}% first run did not hold: {reason}"
            break
        if accepted:
            self.best_metrics = {key: statistics.fmean(run[key] for run in cands)
                                 for key in HIGHER_IS_BETTER if all(key in run for run in cands)}
        self.record({"role": "verdict", "lever": lever, "value": change, "metrics": cands[-1],
                     "samples": self.samples, "verdict": "accepted" if accepted else "rejected",
                     "reason": reason, "gain": round(gain, 4), "runs": len(cands)})
        return accepted, gain, reason

    def sweep(self, entry, changes):
        """Every value at every concurrency; the best workload-weighted value
        then faces its controls."""
        table = {}
        for change in changes:
            if self.stopping:
                break
            if not self.budget_fits(self.apply_cost() + self.run_cost("weighted")):
                self.budget_cut = True
                break
            doc = self.with_change(self.best, change)
            try:
                self.put_doc(doc, f"tune {self.run_id} sweep {entry['lever']}")
            except TuneError as error:
                reason = f"did not load: {error.message}"
                table[json.dumps(change)] = {"error": reason, "failed": True}
                self.failed.append({"lever": entry["lever"], "value": change, "reason": reason})
                self.record({"role": "verdict", "lever": entry["lever"], "value": change,
                             "metrics": None, "samples": self.samples, "verdict": "failed",
                             "reason": reason})
                continue
            metrics, problem = self.measure("weighted")
            if problem:
                table[json.dumps(change)] = {"error": problem}
                continue
            table[json.dumps(change)] = metrics
            self.record({"role": "sweep", "lever": entry["lever"], "value": change, "metrics": metrics,
                         "samples": self.samples})
        self.restore_best("control")
        self.last_control = None
        scored = [(json.loads(k), v["weighted"]) for k, v in table.items() if "weighted" in v]
        self.sweep_tables[entry["lever"]] = table
        if not scored:
            return None
        best_change, _ = max(scored, key=lambda item: item[1])
        current = {key: (self.best.get("load") or {}).get(key) for key in best_change}
        if current == best_change:
            return None
        return best_change

    # ------------------------------------------------------------ the run

    def preflight(self):
        self.resolve()
        self.build, self.model_file, self.listing_row = self.engine_identity()
        self.plan = self.levers()
        lane = self.lane_state()
        if lane and lane.get("experiments") is False:
            raise TuneError(FAIL, f"lane `{self.lane}` does not allow experiments "
                                  "(local_executors[].experiments: false); nothing was held or woken")
        state = (lane or {}).get("state", "unknown")
        self.asleep = state not in ("healthy", "draining", "idle_countdown")
        if self.asleep and not self.args.wake and not self.args.dry_run:
            raise TuneError(FAIL, f"the machine of {self.target} is {state.replace('_', ' ')}; "
                                  "add --wake to wake it for the run")
        if lane and (lane.get("in_flight") or lane.get("queue_depth")):
            raise TuneError(FAIL, f"lane {self.lane} has requests in flight; tune when it is idle")
        self.host = (lane or {}).get("host") or self.lane

    def dry_run(self):
        load = self.best.get("load") or {}
        rows = []
        for entry in self.plan:
            changes, why = self.candidates(entry, load, self.listing_row)
            rows.append({"lever": entry["lever"], "metric": entry.get("metric"), "gate": entry.get("gate"),
                         "kind": entry.get("kind", "ab"), "candidates": changes, "skipped": why,
                         "note": entry["note"]})
        result = {"target": self.target, "run": "dry-run", "levers": rows, "notes": self.notes,
                  "samples": self.samples, "concurrency": self.concurrency,
                  "budget_s": self.budget, "engine_build": self.build, "model_file": self.model_file}
        if self.args.json:
            print(json.dumps(result, indent=2), file=self.out)
            return OK
        self.log(f"sb ai tune {self.target} (dry run: nothing is changed)")
        for row in rows:
            what = row["skipped"] or ", ".join(json.dumps(c) for c in row["candidates"]) or "nothing to try"
            self.log(f"  {row['lever']:<18} {row['metric'] or '-':<9} gate {row['gate'] or '-':<7} {what}")
        for note in self.notes:
            self.log(f"  note: {note}")
        return OK

    def take_hold_and_lock(self):
        ttl = min(int(self.budget + 900), 14400)
        if self.asleep:
            # The lock never wakes anything and refuses a quiet window and a
            # lane with experiments off: on a sleeping machine it comes
            # first, so a refusal leaves the machine asleep.
            self.take_lock(ttl, None)
            self.take_hold(ttl, wake=bool(self.args.wake))
            self.wait_awake()
        else:
            self.take_hold(ttl, wake=False)
            self.take_lock(ttl, self.hold)
        self.last_beat = time.monotonic()

    def take_hold(self, ttl, wake):
        with stop_signals_deferred():
            hold = self.call("POST", f"/v1/hosts/{urllib.parse.quote(self.host)}/holds",
                             body={"ttl_secs": ttl, "reason": f"sb ai tune {self.target}", "wake": wake,
                                   # The gateway ends it by the next quiet
                                   # window, so a run cut off before its lock
                                   # leaves no hold reaching into the window.
                                   "until_quiet_window": True},
                             doing="holding the machine")
            self.hold = hold.get("id")

    def take_lock(self, ttl, hold_id):
        body = {"target": self.target, "ttl_secs": ttl, "reason": self.run_id}
        if hold_id:
            body["hold_id"] = hold_id
        with stop_signals_deferred():
            try:
                lock = self.call("POST", "/v1/optimize/locks", body=body,
                                 doing="taking the lane's tuning lock")
            except TuneError as error:
                raise TuneError(FAIL, error.message.replace("taking the lane's tuning lock: ", "")) from None
            self.lock_id = lock.get("id")

    def wait_awake(self, timeout=600):
        deadline = time.monotonic() + timeout
        poll = float(getattr(self.args, "poll_secs", None) or 2.0)
        while time.monotonic() < deadline:
            lane = self.lane_state()
            # A machine woken by our hold, with no request yet, counts down to
            # idle (the hold keeps it on): it is up, as `healthy` is.
            if lane and lane.get("state") in READY_STATES:
                return
            time.sleep(poll)
        raise TuneError(FAIL, f"the machine of {self.target} did not come up within {timeout} s")

    def release(self):
        if self.lock_id:
            try:
                self.call("DELETE", f"/v1/optimize/locks/{self.lock_id}", doing="releasing the lock")
            except TuneError as error:
                self.say(error.message)
            self.lock_id = None
        if self.hold:
            try:
                self.call("DELETE", f"/v1/hosts/{urllib.parse.quote(self.host)}/holds/{self.hold}",
                          doing="releasing the hold")
            except TuneError as error:
                self.say(error.message)
            self.hold = None

    def ceiling(self):
        try:
            report = self.call("GET", "/v1/optimize/report?model=" + urllib.parse.quote(self.target, safe=""),
                               doing="reading the speed report")
        except TuneError:
            return {}
        for row in report.get("targets") or []:
            if row.get("target") == self.target:
                return row.get("ceiling") or {}
        return {}

    def noise_run(self, runs):
        metrics, problem = self.measure(*self.noise_metrics)
        if problem:
            raise TuneError(FAIL, f"the baseline run failed: {problem}")
        self.record({"role": "baseline", "lever": "baseline", "value": None, "metrics": metrics,
                     "samples": self.samples, "noise_run": len(runs) + 1})
        runs.append(metrics)
        self.sample_counts.append(self.samples)

    def measure_noise(self, metric):
        """Before any lever: NOISE_RUNS runs of the same (current) settings,
        measuring every metric the plan judges or holds (`noise_metrics`).
        Their SD of one run, per metric, is the noise every later verdict
        starts from (`noise_sds`). Over the floor: more samples per run
        when the budget allows (`raise_samples`) and NOISE_MORE more runs,
        measured again over all of them. The baseline is the runs' median;
        the last run is the first control. Stops between runs when the
        budget ran out: fewer than NOISE_RUNS runs leave the run untuned."""
        runs = []
        started = self.clock()
        for _ in range(NOISE_RUNS):
            if runs and self.out_of_budget():
                break
            self.noise_run(runs)
        self.noise = noise_sds(runs)
        if len(runs) == NOISE_RUNS and self.noise.get(metric, 0.0) > NOISE_FLOOR:
            per_sample = (self.clock() - started) / (len(runs) * (self.samples + 1))
            self.raise_samples(per_sample, metric)
            for _ in range(NOISE_MORE):
                if self.out_of_budget():
                    break
                self.noise_run(runs)
            self.noise = noise_sds(runs)
        self.noise_data = runs
        self.noise_runs = len(runs)
        self.baseline = {key: median([run[key] for run in runs]) for key in runs[0]
                         if key in HIGHER_IS_BETTER and all(key in run for run in runs)}
        self.last_control = runs[-1]
        if len(runs) < NOISE_RUNS:
            self.untuned_reason = (f"the budget ran out while measuring the noise ({len(runs)} of "
                                   f"{NOISE_RUNS} runs); nothing changed")
            return
        self.log(f"  noise ±{self.noise.get(metric, 0.0) * 100:.1f}% (SD of one run) on {metric} "
                 f"between {len(runs)} runs of the same settings ({self.samples} samples per run); "
                 f"a win must clear about +{WIN_SD * self.noise_sd(metric) * 100:.0f}%")

    def raise_samples(self, per_sample, metric):
        """More samples per run, when the noise runs' SD is over the floor
        and the budget allows. The median of n samples narrows the noise
        inside a run about as 1/sqrt(n), so (SD / floor)^2 times the samples
        would bring that part to the floor. Drift between runs (heat,
        other work on the machine) does not narrow with samples, so the
        count is capped at twice the requested samples (MAX_SAMPLES at
        most), and fits when half the remaining budget pays the plan's runs
        (NOISE_MORE more noise runs, then per lever about three runs on a
        quiet machine and nine on a noisy one, at the time one sample took
        so far). The noise is measured again after."""
        sd = self.noise.get(metric, 0.0)
        want = math.ceil(self.samples * (sd / NOISE_FLOOR) ** 2)
        levers = max(1, len([entry for entry in self.plan if entry.get("kind") != "advice"]))
        # Rounds per lever as the judge counts them: two on a machine
        # noisier than REPEAT_ABOVE by its own (widened) SD.
        runs = NOISE_MORE + (9 if self.noise_sd(metric) > REPEAT_ABOVE else 3) * levers
        remaining = max(0.0, self.budget - self.elapsed())
        fits = int(remaining * 0.5 / (runs * per_sample)) if per_sample > 0 else want
        new = min(want, fits, 2 * self.requested_samples, MAX_SAMPLES)
        if new > self.samples:
            self.log(f"  runs differ by ±{sd * 100:.1f}% (SD) on {metric}: {new} samples "
                     f"per run now (was {self.samples})")
            self.samples = new

    def tune(self):
        stop = "every lever was tried"
        past = self.history()
        baseline_metric = "prefill" if self.workload == "read" else "decode"
        self.current_metric = baseline_metric
        # The noise runs measure every metric a planned lever judges or
        # holds (load time aside: it unloads the model three times), so
        # each has its own measured noise.
        used = {entry.get("metric") for entry in self.plan} & {"prefill", "aggregate", "weighted"}
        self.noise_metrics = (baseline_metric, *sorted(used - {baseline_metric}))
        self.log(f"sb ai tune {self.target}: run {self.run_id}, engine {self.build}, "
                 f"{self.samples} samples per run, budget {self.budget // 60} min")
        self.measure_noise(baseline_metric)
        if self.untuned_reason:
            return self.untuned_reason
        # Judged by the SD the judge uses (floored, and widened for what a
        # few runs cannot pin down): a run stops where no win could clear
        # it. The message names the SD the runs measured.
        sd = self.noise.get(baseline_metric, 0.0)
        widened = self.noise_sd(baseline_metric)
        if widened > TOO_NOISY:
            # A kept win would have to clear about 1.7 times this noise
            # with room to spare: nothing the levers do could show.
            self.untuned_reason = (
                f"too noisy to tune: noise ±{sd * 100:.1f}% (SD of one run) on {baseline_metric} "
                f"between {self.noise_runs} runs of the same settings, ±{widened * 100:.1f}% "
                f"allowing for so few runs (at most "
                f"±{TOO_NOISY * 100:.0f}% can be tuned); nothing was changed")
            return self.untuned_reason
        started = self.clock()
        greedy = self.run_parallel([lambda: self.chat(GREEDY_PROMPT, 64, temperature=0, fresh=False)])[0]
        # The greedy gate asks the same: what it costs.
        self.gate_secs.setdefault("greedy", []).append(self.clock() - started)
        self.greedy_reference = greedy.get("text")
        self.ceiling_row = self.ceiling()
        flat = 0
        for entry in self.plan:
            if self.out_of_budget() or self.budget_cut:
                stop = "the budget ran out"
                break
            if self.stopping:
                stop = "interrupted"
                break
            self.heartbeat()
            if entry.get("kind") == "advice":
                self.advice(entry)
                continue
            load = self.best.get("load") or {}
            changes, why = self.candidates(entry, load, self.listing_row)
            if why:
                self.notes.append(f"{entry['lever']}: skipped, {why}")
                continue
            fresh = []
            for change in changes:
                earlier = self.rejected_before(past, entry["lever"], change)
                if earlier:
                    self.notes.append(f"{entry['lever']} {json.dumps(change)}: skipped, rejected in "
                                      f"{earlier.get('run_id')} ({earlier.get('reason')}); engine build "
                                      "and model file unchanged")
                    continue
                fresh.append(change)
            if not fresh:
                continue
            self.current_lever, self.current_metric = entry["lever"], entry["metric"]
            measured = kept = False
            if entry.get("kind") == "sweep":
                failed_before = len(self.failed)
                choice = self.sweep(entry, fresh)
                fresh = [choice] if choice else []
                swept = [v for v in self.sweep_tables.get(entry["lever"], {}).values()
                         if not v.get("failed")]
                measured = bool(swept)
                if not fresh and swept:
                    self.notes.append(f"{entry['lever']}: the current value is best in the sweep")
                elif not fresh and len(self.failed) > failed_before:
                    self.log(f"  {entry['lever']}: FAILED, no value of the sweep loaded")
            for change in fresh:
                if self.stopping:
                    break
                if not self.starts_within_budget(entry):
                    self.budget_cut = True
                    self.notes.append(f"{entry['lever']} {json.dumps(change)}: not started, the "
                                      "budget left could not pay for its runs")
                    break
                accepted, gain, reason = self.experiment(entry, change)
                if accepted is None:
                    # Not measured: says nothing about the lever, so it
                    # neither counts toward the plateau nor ends the run.
                    self.failed.append({"lever": entry["lever"], "value": change, "reason": reason})
                    self.log(f"  {entry['lever']} {json.dumps(change)}: FAILED ({reason})")
                    continue
                measured = True
                self.log(f"  {entry['lever']} {json.dumps(change)}: "
                         f"{'kept' if accepted else 'rejected'} ({reason})")
                if accepted:
                    kept = True
                    self.best = self.with_change(self.best, change)
                    self.put_doc(self.best, f"tune {self.run_id} best")
                    self.last_control = None
                    self.gains.append((entry["lever"], change, gain, reason))
                if self.budget_cut:
                    break
            if self.budget_cut:
                stop = "the budget ran out"
                break
            if kept:
                flat = 0
            elif measured:
                flat += 1
                if flat >= PLATEAU:
                    stop = (f"plateau: {PLATEAU} levers in a row inside the noise "
                            f"(±{self.noise_sd(baseline_metric) * 100:.1f}%)")
                    break
        return stop

    def experiment_fits(self, entry):
        """The budget left pays for the fewest runs that could keep a change:
        a control when none is at hand, the first trial's gate, then one
        round (two on a machine noisier than REPEAT_ABOVE: a win must hold
        twice, by the same SD the judge uses)."""
        metric = entry["metric"]
        rounds = 2 if self.noise_sd(metric) > REPEAT_ABOVE else 1
        cost = rounds * self.round_cost(metric) + self.gate_cost(entry.get("gate"))
        if self.last_control is None or metric not in self.last_control:
            cost += self.run_cost(metric)
        return self.budget_fits(cost)

    def starts_within_budget(self, entry):
        """`experiment_fits`, and when the lever's runs cost is only an
        estimate (no such run measured yet: a load-time run before the
        first), its first control is measured and the budget checked again
        with what that run took. The experiment uses that control. So a
        change never starts on a guess: the run ends at most one run past
        its budget."""
        if not self.experiment_fits(entry):
            return False
        metric = entry["metric"]
        if self.run_secs.get(run_kind((metric,))):
            return True
        self.current_lever, self.current_metric = entry["lever"], metric
        self.control()
        return self.experiment_fits(entry)

    def advice(self, entry):
        """sleep_idle_seconds: weigh swaps against what a load costs."""
        try:
            report = self.call("GET", "/v1/optimize/report?model=" + urllib.parse.quote(self.target, safe=""),
                               doing="reading the speed report")
        except TuneError:
            return
        for row in report.get("targets") or []:
            if row.get("target") != self.target:
                continue
            swaps = row.get("swaps_last_hour") or 0
            current = (self.best.get("load") or {}).get("sleep_idle_seconds")
            if swaps >= 2 and current not in (None, -1):
                self.notes.append(f"sleep_idle_seconds: {swaps} swaps in the last hour; a longer idle "
                                  f"time (now {current}) keeps the model loaded between requests")
            else:
                self.notes.append("sleep_idle_seconds: no change suggested (few swaps)")

    def final_metrics(self):
        """The settings now in force, measured once more when the budget
        pays for it; else the kept change's own candidate runs."""
        if not self.gains:
            return self.baseline
        metric = self.current_metric if self.current_metric != "load_s" else "decode"
        if not self.budget_fits(self.run_cost(metric)):
            metrics = dict(self.best_metrics or {})
        else:
            metrics, problem = self.measure(metric)
            if problem:
                self.notes.append(f"the final measurement failed: {problem}")
                return self.baseline
        for key, value in self.baseline.items():
            metrics.setdefault(key, value)
        return metrics

    def summary(self, stop, final):
        ceiling = self.ceiling_row
        how = f"median of {self.samples}, warm, fresh prompts, through the gateway (client clock)"
        counts = sorted(set(self.sample_counts))
        if len(counts) > 1:
            # The noise rule raised the samples after the first runs.
            first = self.sample_counts.count(counts[0])
            how = (f"median of {self.samples} ({counts[0]} in the first {first} baseline runs), "
                   "warm, fresh prompts, through the gateway (client clock)")
        table = []
        # Decode is measured one request at a time (concurrency 1): a c1
        # figure. The aggregate row is the concurrent one.
        for key, label in (("decode", "decode tok/s (c1, median)"), ("prefill", "prefill tok/s"),
                           ("ttft_ms", "time to first token ms"), ("aggregate", "aggregate tok/s"),
                           ("load_s", "load s")):
            if key not in self.baseline and key not in final:
                continue
            ceil = ceiling.get("decode_tps") if key == "decode" else \
                ceiling.get("prefill_tps") if key == "prefill" else None
            table.append({"metric": label, "baseline": self.baseline.get(key), "now": final.get(key),
                          "ceiling": ceil, "how": how})
        unverified = list(self.unverified) + [
            "greedy equality on one fixed prompt only",
            "needle checks at two depths, not a full long-context evaluation",
            "quality beyond these checks (reasoning, tools) was not measured",
            "numbers include the network and the gateway between this machine and the engine",
        ]
        result = {"target": self.target, "run_id": self.run_id, "engine_build": self.build,
                  "model_file": self.model_file, "tuned": self.untuned_reason is None,
                  "stopped": stop, "table": table,
                  "gains": [{"lever": g[0], "value": g[1], "gain_pct": round(g[2] * 100, 1),
                             "reason": g[3]} for g in self.gains],
                  "sweeps": {k: {kk: {str(c): v for c, v in (vv.get("per_concurrency") or {}).items()}
                                 for kk, vv in t.items()} for k, t in self.sweep_tables.items()},
                  "noise": self.noise_view(),
                  "failed": self.failed,
                  "notes": self.notes, "not_verified": unverified,
                  "settings_revision": self.revision, "undo": f"sb ai revert {self.target} {self.base_revision}",
                  "elapsed_s": round(self.elapsed(), 1)}
        if self.untuned_reason:
            result["reason"] = self.untuned_reason
        return result

    def noise_view(self):
        """The measured noise, for the output and `--json`: `sd_pct` is the
        SD of one run (in % of the runs' mean) on the baseline metric."""
        metric = "prefill" if self.workload == "read" else "decode"
        sd = self.noise.get(metric, 0.0)
        return {"metric": metric, "sd_pct": round(sd * 100, 1),
                "by_metric_sd_pct": {key: round(value * 100, 1) for key, value in sorted(self.noise.items())},
                "runs": self.noise_runs, "samples_per_run": self.samples,
                "win_needs_pct": round(WIN_SD * self.noise_sd(metric) * 100, 1),
                "floor_pct": round(NOISE_FLOOR * 100, 1),
                "repeat_above_pct": round(REPEAT_ABOVE * 100, 1),
                "too_noisy_above_pct": round(TOO_NOISY * 100, 1)}

    def print_summary(self, result):
        if self.args.json:
            print(json.dumps(result, indent=2), file=self.out)
            return
        self.log("")
        self.log(f"{'metric':<24} {'baseline':>10} {'now':>10} {'ceiling':>10}  how measured")
        for row in result["table"]:
            fmt = lambda v: "-" if v is None else f"{v:.1f}"  # noqa: E731
            self.log(f"{row['metric']:<24} {fmt(row['baseline']):>10} {fmt(row['now']):>10} "
                     f"{fmt(row['ceiling']):>10}  {row['how']}")
        for lever, table in result["sweeps"].items():
            self.log(f"{lever} by concurrency (aggregate tok/s):")
            for value, per in table.items():
                cells = ", ".join(f"c{c} {v:.0f}" for c, v in per.items())
                self.log(f"  {value}: {cells}")
        if result["gains"]:
            self.log("kept: " + "; ".join(f"{g['lever']} {json.dumps(g['value'])} ({g['gain_pct']:+.0f}%)"
                                          for g in result["gains"]))
        else:
            self.log("kept: nothing (the settings are as before)")
        for failure in result["failed"]:
            self.log(f"failed (not measured): {failure['lever']} {json.dumps(failure['value'])}: "
                     f"{failure['reason']}")
        noise = result["noise"]
        self.log(f"noise: ±{noise['sd_pct']:.1f}% (SD of one run) on {noise['metric']} between "
                 f"{noise['runs']} runs of the same settings, {noise['samples_per_run']} samples per run "
                 f"(a win must clear about +{noise['win_needs_pct']:.0f}%; floor ±{noise['floor_pct']:.0f}%)")
        self.log(f"stopped: {result['stopped']}")
        for note in result["notes"]:
            self.log(f"note: {note}")
        self.log("not verified: " + "; ".join(result["not_verified"]))
        self.log(f"undo with: {result['undo']}")
        if not result["tuned"]:
            self.log(f"not tuned: {result['reason']}")

    def _stop_signal(self, signum, _frame):
        """SIGTERM and SIGHUP stop the run like Ctrl-C: requests in flight
        finish, the best settings go back, the lock and hold are let go."""
        self.signal_name = signal.Signals(signum).name
        raise KeyboardInterrupt()

    def trap_stop_signals(self):
        """Route SIGTERM and SIGHUP into the Ctrl-C path. Answers the
        handlers to put back."""
        saved = {}
        if threading.current_thread() is not threading.main_thread():
            return saved
        for name in ("SIGTERM", "SIGHUP"):
            signum = getattr(signal, name, None)
            if signum is not None:
                saved[signum] = signal.signal(signum, self._stop_signal)
        return saved

    def run(self):
        self.preflight()
        if self.args.dry_run:
            return self.dry_run()
        saved = self.trap_stop_signals()
        try:
            return self.run_trapped()
        finally:
            for signum, handler in saved.items():
                signal.signal(signum, handler)

    def run_trapped(self):
        code = OK
        stop = None
        try:
            self.take_hold_and_lock()
            stop = self.tune()
            final = self.final_metrics()
        except Halt as halt:
            stop, final, code = halt.reason, self.baseline, halt.code
            self.log(f"sb ai tune {self.target}: {halt.reason}")
        except (Interrupted, KeyboardInterrupt):
            how = getattr(self, "signal_name", None) or "Ctrl-C"
            stop, final, code = f"interrupted ({how})", self.baseline, INTERRUPTED
        finally:
            # Always: the best known settings applied, the lock and hold let go.
            try:
                if self.lock_id or self.hold:
                    note = "restore" if code == INTERRUPTED else "best"
                    self.restore_best_finally(note)
                    if code != INTERRUPTED and self.gains:
                        # The winner as its own revision: `sb ai revert` undoes it.
                        self.put_doc(self.best, f"tune {self.run_id}")
            except (TuneError, Halt) as error:
                self.say(f"restoring the best settings failed: {error.message}")
                code = FAIL
            except KeyboardInterrupt:
                # Only the winner's own revision was cut off: the best
                # settings were already back in place.
                self.say("Ctrl-C while recording the winner as its own revision; "
                         "the best settings are in place")
                code = INTERRUPTED
            finally:
                self.release()
        if stop is None:
            return code
        result = self.summary(stop, final or {})
        self.print_summary(result)
        return code


def print_history(gw, args, fail_types):
    path = f"/v1/models/{urllib.parse.quote(args.model, safe='/')}/settings"
    try:
        view = gw.call("GET", path)
        targets = view.get("targets") or []
        target = targets[0]["target"] if len(targets) == 1 else args.model
        rows = gw.call("GET", "/v1/optimize/experiments?model=" + urllib.parse.quote(target, safe="")).get("data") or []
    except fail_types as error:
        print(f"sb ai: reading the experiment ledger: {getattr(error, 'message', error)}", file=sys.stderr)
        return FAIL
    if args.json:
        print(json.dumps({"target": target, "data": rows}, indent=2))
        return OK
    verdicts = [row for row in rows if row.get("role") == "verdict"]
    if not verdicts:
        print(f"{target}: no experiments yet")
        return OK
    print(f"{target}: {len(verdicts)} experiment(s), newest first")
    for row in verdicts:
        when = time.strftime("%Y-%m-%d %H:%M", time.localtime((row.get("at_ms") or 0) / 1000))
        print(f"  {when}  {row.get('run_id')}  {row.get('lever')} {json.dumps(row.get('value'))}: "
              f"{row.get('verdict')} ({row.get('reason')})  engine {row.get('engine_build')}")
    return OK


def add_arguments(parser):
    parser.add_argument("model")
    parser.add_argument("--levers", default="")
    parser.add_argument("--budget", default="30m")
    parser.add_argument("--wake", action="store_true")
    parser.add_argument("--workload", choices=("chat", "read", "mixed"), default="chat")
    parser.add_argument("--concurrency", default="1")
    parser.add_argument("--samples", type=int, default=MIN_SAMPLES)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--history", action="store_true")
    parser.add_argument("--json", action="store_true")


def run(gw, args, fail_types, poll_secs=None):
    """`sb ai tune` with parsed arguments and a gateway client."""
    if args.history:
        return print_history(gw, args, fail_types)
    args.poll_secs = poll_secs
    try:
        return Tuner(gw, args, fail_types).run()
    except TuneError as error:
        print(f"sb ai: {error.message}", file=sys.stderr)
        return error.code
