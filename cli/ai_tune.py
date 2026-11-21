#!/usr/bin/env python3
"""sb ai tune: controlled speed experiments on one local model, through the
gateway.

The loop of the inference-optimisation skill
(https://github.com/sybil-solutions/inference-optimisation-skill), run on a
live gateway: measure a baseline, read the ceiling, change one load setting
at a time from the current best, and judge each change with
control / candidate / control runs. A change is kept only when it beats both
neighbouring controls on its own metric and no other measured metric is
more than 3% worse than the mean of the two controls (than the worse
control, when the two differ by more than 3% on that metric); a win of more
than 20%, or a run whose two controls differ by more than 3% on the judged
metric (noisy), is repeated once first and must win again (by more than the
noise, for a surprising win).
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

import hashlib
import json
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

NOISE = 0.05        # a change inside +-5% is noise (run-to-run noise is 5-10%)
REGRESSION = 0.03   # another metric worse by more than 3% rejects a change
SURPRISE = 0.20     # a win over 20% is repeated once before it is kept
PLATEAU = 3         # three levers in a row inside the noise: stop
MIN_SAMPLES = 6     # decode samples per run (the median is used)
HEARTBEAT_S = 30    # renew the lock and read the quiet window at most this often
APPLY_WAIT_S = 600  # how long a held-back apply (model busy) may stay pending
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


class Tuner:
    def __init__(self, gw, args, fail_types, out=sys.stdout, err=sys.stderr):
        self.gw = gw
        self.args = args
        self.fail_types = fail_types
        self.out = out
        self.err = err
        self.run_id = "tune_" + uuid.uuid4().hex[:10]
        self.samples = max(MIN_SAMPLES, int(getattr(args, "samples", None) or MIN_SAMPLES))
        self.concurrency = parse_concurrency(args.concurrency)
        self.budget = parse_budget(args.budget)
        self.workload = args.workload
        self.started = time.monotonic()
        self.lock_id = None
        self.hold = None
        self.stopping = False
        self.rows = []
        self.notes = []
        self.unverified = []
        self.gains = []
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

    def measure(self, metric):
        """One run: the metric the lever is judged on plus decode and time to
        first token, each the median over the run's samples. Fresh prompts
        (no cache hits). The first request after a load is discarded."""
        if self.stopping:
            raise Interrupted()
        self.heartbeat()
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
        if metric == "prefill" or self.workload in ("read", "mixed"):
            reads = []
            for _ in range(self.samples):
                reads.extend(self.run_parallel([lambda: self.chat(READ_PROMPT, 16)]))
            problem = self.check_answers(reads)
            if problem:
                return None, problem
            out["prefill"] = median([a["prompt_tps"] for a in reads])
        if metric in ("aggregate", "weighted"):
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
        if metric == "load_s":
            out["load_s"] = self.load_time()
        return out, None

    def load_time(self):
        path = f"/v1/models/{urllib.parse.quote(self.target, safe='/')}"
        times = []
        for _ in range(3):
            self.call("POST", path + "/unload", body={}, doing="unloading")
            started = time.monotonic()
            self.call("POST", path + "/load", body={"wake": False}, timeout=900, doing="loading")
            times.append(time.monotonic() - started)
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
        return time.monotonic() - self.started

    def out_of_budget(self):
        return self.elapsed() >= self.budget

    def verdict(self, metric, c1, cand, c2):
        """(accepted, gain, reason, noisy) by the skill's rules. Every other
        metric is held to the mean of the two controls, or to the worse
        control when the two differ by more than 3% on it. `noisy` when the
        two controls differ by more than 3% on the judged metric, so the
        run says too little and is repeated (the repeat rule)."""
        higher = HIGHER_IS_BETTER[metric]
        a, b, x = c1[metric], c2[metric], cand[metric]
        base = (a + b) / 2
        gain = (x / base - 1) if higher else (base / x - 1 if x > 0 else 0)
        noisy = base > 0 and abs(a - b) / base > REGRESSION
        beats = x > max(a, b) if higher else x < min(a, b)
        if not beats:
            return False, gain, (f"{metric} {x:.1f} did not beat both controls ({a:.1f}, {b:.1f})"), False
        for other in ("decode", "ttft_ms", "prefill", "aggregate"):
            if other == metric or other not in cand or other not in c1 or other not in c2:
                continue
            mean = (c1[other] + c2[other]) / 2
            if mean <= 0:
                continue
            reference, against = mean, "the controls' mean"
            if abs(c1[other] - c2[other]) / mean > REGRESSION:
                # The controls disagree on a side metric: hold it to the
                # worse control, not the run's verdict to a repeat.
                pick = min if HIGHER_IS_BETTER[other] else max
                reference, against = pick(c1[other], c2[other]), "the worse control"
            if reference <= 0:
                continue
            if HIGHER_IS_BETTER[other]:
                worse = (reference - cand[other]) / reference
            else:
                worse = (cand[other] - reference) / reference
            if worse > REGRESSION:
                return False, gain, (f"{metric} gained {gain * 100:.0f}% but {other} got "
                                     f"{worse * 100:.0f}% worse than {against} "
                                     "(more than 3%)"), noisy
        if noisy:
            return True, gain, (f"{metric} {gain * 100:+.0f}% over both controls, but the controls "
                                f"differ by more than 3% on {metric}"), True
        return True, gain, f"{metric} {gain * 100:+.0f}% over both controls", False

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

    def trial(self, entry, change):
        """Apply a candidate, gate it and measure it. (metrics, problem)."""
        doc = self.with_change(self.best, change)
        try:
            self.put_doc(doc, f"tune {self.run_id} trial {entry['lever']}")
        except TuneError as error:
            return None, f"did not load: {error.message}"
        problem = self.gate(entry["gate"], doc)
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

    def experiment(self, entry, change, repeat=False):
        """Control, candidate, control. Returns (accepted, gain, reason)."""
        lever, metric = entry["lever"], entry["metric"]
        self.current_lever, self.current_metric = lever, metric
        c1 = self.control()
        cand, problem = self.trial(entry, change)
        self.restore_best("control")
        self.last_control = None
        if problem:
            self.record({"role": "verdict", "lever": lever, "value": change, "metrics": cand,
                         "samples": self.samples, "verdict": "rejected", "reason": problem})
            return False, 0.0, problem
        self.record({"role": "candidate", "lever": lever, "value": change, "metrics": cand,
                     "samples": self.samples})
        c2 = self.control()
        accepted, gain, reason, noisy = self.verdict(metric, c1, cand, c2)
        if noisy and repeat:
            # Noisy twice: the run cannot tell, so nothing changes.
            accepted, reason = False, f"noisy again: {reason}"
        surprising = accepted and gain > SURPRISE
        if (surprising or noisy) and not repeat:
            why = "a surprising win" if surprising else "noisy controls"
            self.log(f"  {lever} {json.dumps(change)}: {gain * 100:+.0f}%, {why}; repeating it once")
            again, gain2, reason2 = self.experiment(entry, change, repeat=True)
            if again and surprising and gain2 < NOISE:
                again, reason2 = False, f"the repeat gained only {gain2 * 100:+.0f}%, inside the noise"
            if not again:
                reason = (f"a {gain * 100:+.0f}% win did not repeat ({reason2})" if accepted
                          else f"{reason}; repeated: {reason2}")
                self.record({"role": "verdict", "lever": lever, "value": change, "verdict": "rejected",
                             "reason": reason, "metrics": cand, "samples": self.samples})
                return False, 0.0, reason
            # The repeat decides: a noisy run rejected the first time is
            # kept when its repeat (with agreeing controls) wins.
            accepted = True
            gain = min(gain, gain2)
            reason = f"{reason}; repeated: {reason2}"
        if not repeat:
            self.record({"role": "verdict", "lever": lever, "value": change, "metrics": cand,
                         "samples": self.samples, "verdict": "accepted" if accepted else "rejected",
                         "reason": reason, "gain": round(gain, 4)})
        return accepted, gain, reason

    def sweep(self, entry, changes):
        """Every value at every concurrency; the best workload-weighted value
        then faces its controls."""
        table = {}
        for change in changes:
            if self.out_of_budget() or self.stopping:
                break
            doc = self.with_change(self.best, change)
            try:
                self.put_doc(doc, f"tune {self.run_id} sweep {entry['lever']}")
            except TuneError as error:
                table[json.dumps(change)] = {"error": error.message}
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
        ttl = int(self.budget + 900)
        hold = self.call("POST", f"/v1/hosts/{urllib.parse.quote(self.host)}/holds",
                         body={"ttl_secs": min(ttl, 14400), "reason": f"sb ai tune {self.target}",
                               "wake": bool(self.args.wake and self.asleep),
                               # The gateway ends it by the next quiet window,
                               # so a run cut off before its lock leaves no
                               # hold reaching into the window.
                               "until_quiet_window": True},
                         doing="holding the machine")
        self.hold = hold.get("id")
        if self.asleep:
            self.wait_awake()
        try:
            lock = self.call("POST", "/v1/optimize/locks",
                             body={"target": self.target, "ttl_secs": min(ttl, 14400),
                                   "reason": self.run_id, "hold_id": self.hold},
                             doing="taking the lane's tuning lock")
        except TuneError as error:
            raise TuneError(FAIL, error.message.replace("taking the lane's tuning lock: ", "")) from None
        self.lock_id = lock.get("id")
        self.last_beat = time.monotonic()

    def wait_awake(self, timeout=600):
        deadline = time.monotonic() + timeout
        poll = float(getattr(self.args, "poll_secs", None) or 2.0)
        while time.monotonic() < deadline:
            lane = self.lane_state()
            if lane and lane.get("state") == "healthy":
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

    def tune(self):
        stop = "every lever was tried"
        past = self.history()
        baseline_metric = "prefill" if self.workload == "read" else "decode"
        self.current_metric = baseline_metric
        self.log(f"sb ai tune {self.target}: run {self.run_id}, engine {self.build}, "
                 f"{self.samples} samples per run, budget {self.budget // 60} min")
        self.baseline, problem = self.measure(baseline_metric)
        if problem:
            raise TuneError(FAIL, f"the baseline run failed: {problem}")
        self.record({"role": "baseline", "lever": "baseline", "value": None,
                     "metrics": self.baseline, "samples": self.samples})
        self.last_control = self.baseline
        greedy = self.run_parallel([lambda: self.chat(GREEDY_PROMPT, 64, temperature=0, fresh=False)])[0]
        self.greedy_reference = greedy.get("text")
        self.ceiling_row = self.ceiling()
        flat = 0
        for entry in self.plan:
            if self.out_of_budget():
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
            best_gain = 0.0
            if entry.get("kind") == "sweep":
                choice = self.sweep(entry, fresh)
                fresh = [choice] if choice else []
                if not fresh:
                    self.notes.append(f"{entry['lever']}: the current value was best in the sweep")
            for change in fresh:
                if self.out_of_budget() or self.stopping:
                    break
                accepted, gain, reason = self.experiment(entry, change)
                self.log(f"  {entry['lever']} {json.dumps(change)}: "
                         f"{'kept' if accepted else 'rejected'} ({reason})")
                if accepted:
                    self.best = self.with_change(self.best, change)
                    self.put_doc(self.best, f"tune {self.run_id} best")
                    self.last_control = None
                    self.gains.append((entry["lever"], change, gain, reason))
                best_gain = max(best_gain, abs(gain) if accepted else 0.0)
            if best_gain < NOISE:
                flat += 1
                if flat >= PLATEAU:
                    stop = f"plateau: {PLATEAU} levers in a row inside the noise"
                    break
            else:
                flat = 0
        return stop

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
        if not self.gains:
            return self.baseline
        metrics, problem = self.measure(self.current_metric if self.current_metric != "load_s" else "decode")
        if problem:
            self.notes.append(f"the final measurement failed: {problem}")
            return self.baseline
        for key, value in self.baseline.items():
            metrics.setdefault(key, value)
        return metrics

    def summary(self, stop, final):
        ceiling = self.ceiling_row
        how = f"median of {self.samples}, warm, fresh prompts, through the gateway (client clock)"
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
        return {"target": self.target, "run_id": self.run_id, "engine_build": self.build,
                "model_file": self.model_file, "stopped": stop, "table": table,
                "gains": [{"lever": g[0], "value": g[1], "gain_pct": round(g[2] * 100, 1),
                           "reason": g[3]} for g in self.gains],
                "sweeps": {k: {kk: {str(c): v for c, v in (vv.get("per_concurrency") or {}).items()}
                               for kk, vv in t.items()} for k, t in self.sweep_tables.items()},
                "notes": self.notes, "not_verified": unverified,
                "settings_revision": self.revision, "undo": f"sb ai revert {self.target} {self.base_revision}",
                "elapsed_s": round(self.elapsed(), 1)}

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
        self.log(f"stopped: {result['stopped']}")
        for note in result["notes"]:
            self.log(f"note: {note}")
        self.log("not verified: " + "; ".join(result["not_verified"]))
        self.log(f"undo with: {result['undo']}")

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
