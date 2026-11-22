#!/usr/bin/env python3
"""sb ai eval: answer quality of local models, measured on suites of real
requests replayed through the gateway.

A suite is a folder: `suite.json` (name, kind, scorer, concurrency, repeats,
min_effect, seed, roles, gate_cases, version_inputs), `cases.jsonl` (one
request per line, exactly as its caller sent it, with what a good answer must
satisfy), optional `labels.jsonl` (gold labels a plug-in reads) and optional
`score.py` (the suite's own scorer). The suite's version is a hash of those
files and of every file `version_inputs` names, so any change to a case, a
label or a rule makes a new version.

Each case is sent `repeats` times, byte for byte, at the suite's own
concurrency, with a per-request settings block that makes the gateway skip
its exact-request cache (every repeat reaches the engine). Each answer is
scored by hard checks (error, empty, cut at the token limit, a loop, invalid
JSON when JSON is expected: score 0) and then by the case's own checks, or by
the suite's plug-in. The run's score is the suite's aggregate with a
bootstrap standard error and 95% interval over case ids (2,000 resamples,
seed 1, so a printout repeats).

"A beats B" pairs the two runs on the case ids both measured: A's lead D must
be larger than both `min_effect` (2 points by default) and two standard
errors of the paired difference. Otherwise it is a tie when two standard
errors are at most 5 points, and "cannot separate, add cases" when they are
more. Fewer than 30 shared cases, or two suite versions: no verdict.

The gateway keeps one row per (target, suite) run: scores, case ids and
counts, never a prompt or an answer. A stored score is current while the
model file, the engine build, the answer-settings hash and the suite version
all still match; `--compare` uses current rows only unless `--any`.

Safety (the tuner's rules): never on a sleeping machine without --wake
(exit 3, nothing sent), never on a lane with `experiments: false` (refused
before any hold), never inside a quiet window. It holds the machine, takes
the lane's tuning lock (other requests go elsewhere meanwhile), loads the
model once, and lets both go at the end, on Ctrl-C, SIGTERM and SIGHUP too.
Between cases a heartbeat renews the lock; a quiet window that began, or a
lock that is gone, stops the run at the next case boundary with a `partial`
row, which no verdict ever uses.

Answers are kept only in the private runs folder (never inside a git work
tree), so `--rescore RUN_ID` can score them again after a label round
without any model call.

Standard library only. The gateway client is `ai_gateway.Gateway`.
"""

import hashlib
import importlib.util
import json
import math
import os
import random
import re
import signal
import stat
import statistics
import sys
import threading
import time
import urllib.parse
import uuid
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from pathlib import Path

try:
    import ai_tune
except ImportError:  # the module sits next to this file
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import ai_tune

OK, FAIL, USAGE, MACHINE_OFF = 0, 1, 2, 3
INTERRUPTED = 130

SUITE_FILES = ("suite.json", "cases.jsonl", "labels.jsonl", "score.py")
BOOTSTRAP = 2000          # resamples for every standard error and interval
MIN_SHARED = 30           # shared case ids a verdict needs
TIE_BAR = 0.05            # two standard errors at most this: a tie, else "add cases"
MIN_EFFECT = 0.02         # the smallest lead that counts (suite.json may raise it)
NONDETERMINISTIC = 0.10   # above this share of cases whose repeats differ: one more repeat
HEARTBEAT_S = 30          # renew the lock and read the quiet window at most this often
PAUSED_WAIT_S = 300       # how long a request retries a paused model (503 model_paused)
REQUEST_TIMEOUT_S = 900
LOAD_TIMEOUT_S = 900
WAKE_TIMEOUT_S = 600
MAX_LOCK_S = 14400
LOCK_HEADER = "x-switchback-tune-lock"
UNKNOWN = "unknown"       # an identity part the gateway does not name
# Lane states of a machine that is up and serves requests.
READY_STATES = ("healthy", "idle_countdown")
ON_STATES = ("healthy", "draining", "idle_countdown", "degraded")

# The gateway's no-text rule for quality rows, checked here too so a row is
# never sent that it would refuse. Every key is a name (NAME, and no text
# word as its start or as one of its `_` parts), except the keys of the
# case-id maps, which are case ids (CASE_ID: lower-case letters and digits in
# at most four parts). A string may sit only in the top-level identity
# fields (STRING_FIELDS), at most 200 characters; any other string, an array
# of strings included, is refused.
FORBIDDEN_KEYS = ("prompt", "response", "text", "content", "message", "answer", "quote", "raw",
                  "body", "secret", "token", "key")
STRING_FIELDS = ("run_id", "target", "suite", "suite_version", "model_file", "engine_build",
                 "settings_hash", "status", "rescored_from", "stop_reason")
MAX_STRING = 200
MAX_ROW = 64 * 1024
ID_MAPS = ("per_case", "case_detail", "case_fail")
NAME = re.compile(r"^[a-z][a-z0-9_]{0,31}$")
CASE_ID = re.compile(r"^[a-z0-9]+(?:[-_.:][a-z0-9]+){0,3}$")
SUITE_NAME = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")


class EvalError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


class Halt(Exception):
    """Stop at a case boundary: a quiet window began (`code` OK), the lock
    is gone or the gateway stopped answering (`code` FAIL). `kind` is the
    typed reason the stored row carries as `stop_reason`."""

    def __init__(self, reason, code, kind):
        super().__init__(reason)
        self.reason = self.message = reason
        self.code = code
        self.kind = kind


class Interrupted(Exception):
    pass


# ---------------------------------------------------------------- paths

def suite_roots(paths, environ):
    """Where suites are looked for, in order: SB_AI_SUITES (`:`-separated)
    or ${XDG_DATA_HOME:-~/.local/share}/switchback/suites, then the
    example suites next to this file."""
    listed = [p for p in (paths.get("SB_AI_SUITES") or "").split(":") if p.strip()]
    if not listed:
        base = environ.get("XDG_DATA_HOME") or os.path.join(os.path.expanduser("~"), ".local", "share")
        listed = [os.path.join(base, "switchback", "suites")]
    listed.append(os.path.join(os.path.dirname(os.path.abspath(__file__)), "suites"))
    out = []
    for path in listed:
        real = os.path.realpath(os.path.expanduser(path))
        if real not in out:
            out.append(real)
    return out


def runs_folder(paths, environ):
    """Where answers are kept: SB_AI_EVAL_RUNS or
    ${XDG_STATE_HOME:-~/.local/state}/switchback/eval-runs. Never inside a
    git work tree, so answers never land in a repository."""
    folder = paths.get("SB_AI_EVAL_RUNS") or ""
    if not folder:
        base = environ.get("XDG_STATE_HOME") or os.path.join(os.path.expanduser("~"), ".local", "state")
        folder = os.path.join(base, "switchback", "eval-runs")
    folder = os.path.realpath(os.path.expanduser(folder))
    probe = folder
    while True:
        if os.path.exists(os.path.join(probe, ".git")):
            raise EvalError(USAGE, f"the runs folder {folder} is inside a git work tree ({probe}); "
                                   "answers must stay out of repositories: set SB_AI_EVAL_RUNS "
                                   "to a private folder")
        parent = os.path.dirname(probe)
        if parent == probe:
            return folder
        probe = parent


# ---------------------------------------------------------------- suites

def read_json_file(path):
    try:
        with open(path, "rb") as handle:
            return json.loads(handle.read())
    except FileNotFoundError:
        raise EvalError(USAGE, f"{path} is missing") from None
    except (OSError, ValueError) as error:
        raise EvalError(USAGE, f"{path}: {error}") from None


def read_jsonl(path, required=True):
    rows = []
    try:
        with open(path, encoding="utf-8") as handle:
            for number, line in enumerate(handle, 1):
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                except ValueError as error:
                    raise EvalError(USAGE, f"{path} line {number}: {error}") from None
                if not isinstance(row, dict):
                    raise EvalError(USAGE, f"{path} line {number}: not a JSON object")
                rows.append(row)
    except FileNotFoundError:
        if required:
            raise EvalError(USAGE, f"{path} is missing") from None
    except OSError as error:
        raise EvalError(USAGE, f"{path}: {error.strerror}") from None
    return rows


def suite_version(path):
    """The first 12 hex of SHA-256 over the suite's files (suite.json,
    cases.jsonl, labels.jsonl, score.py: each with its name, in that order,
    missing ones skipped) and every file `version_inputs` names (absolute or
    relative to the suite). A missing version input is a usage error."""
    path = Path(path)
    meta = read_json_file(path / "suite.json")
    digest = hashlib.sha256()

    def add(name, file):
        data = file.read_bytes()
        digest.update(name.encode() + b"\0" + str(len(data)).encode() + b"\0")
        digest.update(data)

    for name in SUITE_FILES:
        if (path / name).is_file():
            add(name, path / name)
    inputs = meta.get("version_inputs") or [] if isinstance(meta, dict) else []
    if not isinstance(inputs, list):
        raise EvalError(USAGE, f"suite {path.name}: version_inputs must be a list of paths")
    for item in inputs:
        file = Path(item) if os.path.isabs(str(item)) else path / str(item)
        if not file.is_file():
            raise EvalError(USAGE, f"suite {path.name}: version input {item} is missing")
        add("input:" + str(item), file)
    return digest.hexdigest()[:12]


def plugin_problem(file, roots):
    """Why a suite's score.py may not be loaded, or None: it must sit inside
    a suites root, belong to the current user, and be writable by nobody
    else (nor may its folder be)."""
    real = os.path.realpath(file)
    if not any(real.startswith(root.rstrip(os.sep) + os.sep) for root in roots):
        return f"{file} is not inside a suites folder"
    for item in (real, os.path.dirname(real)):
        info = os.stat(item)
        if hasattr(os, "getuid") and info.st_uid != os.getuid():
            return f"{item} belongs to another user"
        if info.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
            return f"{item} is writable by group or others"
    return None


class Suite:
    def __init__(self, name, path, roots):
        self.name = name
        self.path = Path(path)
        self.roots = roots
        self.meta = read_json_file(self.path / "suite.json")
        if not isinstance(self.meta, dict):
            raise EvalError(USAGE, f"{self.path / 'suite.json'} is not a JSON object")
        self.kind = self.meta.get("kind", "chat")
        if self.kind != "chat":
            raise EvalError(USAGE, f"suite {name}: kind {self.kind} is not measured yet (chat only)")
        self.scorer = self.meta.get("scorer", "checks")
        if self.scorer not in ("checks", "plugin"):
            raise EvalError(USAGE, f"suite {name}: scorer must be checks or plugin")
        self.cases = read_jsonl(self.path / "cases.jsonl")
        self.labels = read_jsonl(self.path / "labels.jsonl", required=False)
        seen = set()
        for case in self.cases:
            case_id = case.get("id")
            if not is_case_id(case_id):
                raise EvalError(USAGE, f"suite {name}: case id {case_id!r} must be 1-64 lower-case "
                                       "letters and digits, in at most four parts joined by - _ . :")
            if case_id in seen:
                raise EvalError(USAGE, f"suite {name}: case id {case_id} appears twice")
            seen.add(case_id)
            request = case.get("request")
            if not isinstance(request, dict) or not isinstance(request.get("messages"), list):
                raise EvalError(USAGE, f"suite {name}: case {case_id} needs a request with messages")
            if "model" in request:
                raise EvalError(USAGE, f"suite {name}: case {case_id}: the request carries no model "
                                       "(the run adds it)")
        if not self.cases:
            raise EvalError(USAGE, f"suite {name} has no cases")
        self.version = suite_version(self.path)
        self.repeats = self.int_setting("repeats", 2, 1, 10)
        self.concurrency = self.int_setting("concurrency", 1, 1, 64)
        self.seed = self.int_setting("seed", 1, 0, 2**31 - 1)
        self.min_effect = float(self.meta.get("min_effect", MIN_EFFECT))
        self.plugin = None
        if self.scorer == "plugin":
            self.plugin = self.load_plugin()

    def int_setting(self, key, default, low, high):
        value = self.meta.get(key, default)
        if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
            raise EvalError(USAGE, f"suite {self.name}: {key} must be a whole number from {low} to {high}")
        return value

    def load_plugin(self):
        file = self.path / "score.py"
        if not file.is_file():
            raise EvalError(USAGE, f"suite {self.name}: scorer plugin needs {file}")
        problem = plugin_problem(file, self.roots)
        if problem:
            raise EvalError(USAGE, f"suite {self.name}: refusing to load its scorer: {problem}")
        spec = importlib.util.spec_from_file_location(f"sb_suite_{self.name.replace('-', '_')}", file)
        module = importlib.util.module_from_spec(spec)
        try:
            spec.loader.exec_module(module)
        except Exception as error:  # noqa: BLE001 - the suite's own code
            raise EvalError(USAGE, f"suite {self.name}: loading score.py failed: {error}") from None
        if not callable(getattr(module, "score", None)):
            raise EvalError(USAGE, f"suite {self.name}: score.py has no score(case, answer)")
        if hasattr(module, "set_labels"):
            module.set_labels(self.labels)
        return module

    def score(self, case, answer):
        if self.plugin is None:
            return check_case(case, answer)
        try:
            result = self.plugin.score(case, answer)
        except Exception as error:  # noqa: BLE001 - the suite's own code
            raise EvalError(FAIL, f"suite {self.name}: score() failed on {case['id']}: {error}") from None
        return valid_result(result, self.name, case["id"])

    def aggregate(self, results):
        """The suite's score over per-(case, repeat) results."""
        if not results:
            return 0.0
        if self.plugin is not None and callable(getattr(self.plugin, "aggregate", None)):
            value = self.plugin.aggregate(results)
            if not isinstance(value, (int, float)) or isinstance(value, bool) or not 0 <= value <= 1:
                raise EvalError(FAIL, f"suite {self.name}: aggregate() must answer a number in 0..1")
            return float(value)
        return mean_of_cases(results)


def is_name(key):
    """A name a row may use for a count, a hard-fail reason or a field:
    [a-z][a-z0-9_], at most 32, not named like text."""
    return (isinstance(key, str) and NAME.match(key) is not None
            and not key.startswith(FORBIDDEN_KEYS)
            and not any(part in FORBIDDEN_KEYS for part in key.split("_")))


def is_case_id(key):
    return isinstance(key, str) and len(key) <= 64 and CASE_ID.match(key) is not None


def valid_result(result, suite, case_id):
    """A plug-in's result, checked: a score in 0..1, a hard-fail reason that
    is a name (or null) and a detail that maps names to numbers. A reason or
    a name that could carry text stops the run before anything is stored."""
    if not isinstance(result, dict):
        raise EvalError(FAIL, f"suite {suite}: score() on {case_id} did not answer an object")
    score = result.get("score")
    if isinstance(score, bool) or not isinstance(score, (int, float)) or not 0 <= score <= 1:
        raise EvalError(FAIL, f"suite {suite}: score() on {case_id} must answer a score in 0..1")
    hard = result.get("hard_fail")
    if hard is not None and not is_name(hard):
        raise EvalError(FAIL, f"suite {suite}: hard_fail on {case_id} must be a short name "
                              "([a-z][a-z0-9_], at most 32, not named like text) or null")
    detail = result.get("detail") or {}
    if not isinstance(detail, dict) or not all(
            isinstance(v, (int, float)) and not isinstance(v, bool) for v in detail.values()):
        raise EvalError(FAIL, f"suite {suite}: detail on {case_id} must map names to numbers")
    if not all(is_name(k) for k in detail):
        raise EvalError(FAIL, f"suite {suite}: a detail key on {case_id} is not a short name "
                              "([a-z][a-z0-9_], at most 32, not named like text)")
    return {"score": float(score), "hard_fail": hard, "detail": dict(detail)}


def mean_of_cases(results):
    """The mean over cases of each case's mean over its repeats."""
    by_case = {}
    for result in results:
        by_case.setdefault(result["case_id"], []).append(result["score"])
    return statistics.fmean(statistics.fmean(scores) for scores in by_case.values())


def find_suite(name, roots):
    if not name:
        raise EvalError(USAGE, "name the suite with --suite NAME (sb ai eval --list shows them)")
    if not SUITE_NAME.match(name):
        raise EvalError(USAGE, f"--suite {name}: a suite is named, not given as a path")
    for root in roots:
        path = os.path.join(root, name)
        if os.path.isfile(os.path.join(path, "suite.json")):
            return Suite(name, path, roots)
    raise EvalError(USAGE, f"no suite {name} in {', '.join(roots)}")


def list_suites(roots):
    rows = []
    seen = set()
    for root in roots:
        try:
            names = sorted(os.listdir(root))
        except OSError:
            continue
        for name in names:
            path = os.path.join(root, name)
            if name in seen or not os.path.isfile(os.path.join(path, "suite.json")):
                continue
            seen.add(name)
            row = {"name": name, "root": root}
            try:
                meta = read_json_file(os.path.join(path, "suite.json"))
                row.update({"version": suite_version(path), "kind": meta.get("kind", "chat"),
                            "scorer": meta.get("scorer", "checks"),
                            "cases": len(read_jsonl(os.path.join(path, "cases.jsonl"))),
                            "roles": meta.get("roles") or []})
            except EvalError as error:
                row["error"] = error.message
            rows.append(row)
    return rows


# ---------------------------------------------------------------- checks

def parse_json_answer(text):
    """The answer as JSON; a single ``` fence around it is allowed."""
    stripped = (text or "").strip()
    fenced = re.match(r"^```[A-Za-z]*\s*\n(.*)\n```$", stripped, re.S)
    if fenced:
        stripped = fenced.group(1)
    return json.loads(stripped)


def hard_fail(answer, expect_json=False):
    """The reason an answer scores 0 whatever it says, or None."""
    if answer.get("error"):
        return "error"
    text = answer.get("text") or ""
    if not text.strip():
        return "empty"
    if answer.get("finish") == "length":
        return "cut"
    if ai_tune.looping(text):
        return "loop"
    if expect_json:
        try:
            parse_json_answer(text)
        except ValueError:
            return "json"
    return None


def squash(text):
    return " ".join(str(text).split())


def normalize(value, how):
    if how == "int":
        match = re.search(r"-?\d[\d,]*(?:\.\d+)?", str(value))
        if not match:
            return None
        try:
            return int(round(float(match.group(0).replace(",", ""))))
        except ValueError:
            return None
    if how == "upper":
        return squash(value).upper()
    return squash(value).casefold()


JSON_TYPES = {"string": str, "number": (int, float), "integer": int, "boolean": bool,
              "array": list, "object": dict, "null": type(None)}


def json_type_ok(value, name):
    kind = JSON_TYPES.get(name)
    if kind is None:
        return False
    if name in ("number", "integer") and isinstance(value, bool):
        return False
    return isinstance(value, kind)


def scored_checks(expect, text):
    """(passed, total) over the case's scored checks."""
    passed = total = 0
    parsed, parsed_ok = None, False
    try:
        parsed, parsed_ok = parse_json_answer(text), True
    except ValueError:
        pass
    for check in expect.get("checks") or []:
        if not isinstance(check, dict):
            continue
        fold = (lambda s: s.casefold()) if check.get("ignore_case") else (lambda s: s)
        if "contains_all" in check:
            total += 1
            passed += all(fold(str(item)) in fold(text) for item in check["contains_all"])
        elif "contains_none" in check:
            total += 1
            passed += not any(fold(str(item)) in fold(text) for item in check["contains_none"])
        elif "regex" in check:
            total += 1
            flags = re.I if check.get("ignore_case") else 0
            passed += re.search(check["regex"], text, flags) is not None
        elif "exact" in check:
            total += 1
            passed += normalize(text, check.get("as", "text")) == normalize(check["exact"],
                                                                           check.get("as", "text"))
        elif "json_schema" in check:
            total += 1
            schema = check["json_schema"] or {}
            ok = parsed_ok and json_type_ok(parsed, schema.get("type", "object"))
            if ok and isinstance(parsed, dict):
                ok = all(key in parsed for key in schema.get("required") or [])
                for key, spec in (schema.get("properties") or {}).items():
                    if ok and key in parsed and isinstance(spec, dict) and "type" in spec:
                        ok = json_type_ok(parsed[key], spec["type"])
                if ok and schema.get("additional") is False:
                    ok = set(parsed) <= set(schema.get("properties") or {}) | set(schema.get("required") or [])
            passed += bool(ok)
        elif "fields" in check:
            for field, want in (check["fields"] or {}).items():
                total += 1
                if not (parsed_ok and isinstance(parsed, dict) and field in parsed):
                    continue
                spec = want if isinstance(want, dict) else {"equals": want}
                how = spec.get("as", "text")
                got = parsed[field]
                if how == "loose":
                    a, b = normalize(got, "text"), normalize(spec.get("equals"), "text")
                    passed += bool(a) and bool(b) and (a in b or b in a)
                else:
                    passed += normalize(got, how) == normalize(spec.get("equals"), how)
        elif "length" in check:
            total += 1
            rule = check["length"] or {}
            words, chars = len(text.split()), len(text.strip())
            passed += (words >= rule.get("min_words", 0) and words <= rule.get("max_words", words)
                       and chars >= rule.get("min_chars", 0) and chars <= rule.get("max_chars", chars))
    return passed, total


def check_case(case, answer):
    """The built-in scorer: hard checks, then the share of scored checks
    that pass (1 when the case has none)."""
    expect = case.get("expect") or {}
    reason = hard_fail(answer, bool(expect.get("json")))
    if reason:
        return {"score": 0.0, "hard_fail": reason, "detail": {}}
    passed, total = scored_checks(expect, answer.get("text") or "")
    return {"score": passed / total if total else 1.0, "hard_fail": None,
            "detail": {"checks": total, "passed": passed}}


# ---------------------------------------------------------------- statistics

def by_case(results):
    out = {}
    for result in results:
        out.setdefault(result["case_id"], []).append(result)
    return out


def percentile(values, share):
    ordered = sorted(values)
    if not ordered:
        return None
    index = min(len(ordered) - 1, max(0, int(math.ceil(share * len(ordered))) - 1))
    return ordered[index]


def bootstrap(cases, aggregate):
    """(standard error, (low, high)) of `aggregate` over case-id resamples."""
    ids = sorted(cases)
    if len(ids) < 2:
        return None, (None, None)
    rng = random.Random(1)
    values = []
    for _ in range(BOOTSTRAP):
        sample = [rng.choice(ids) for _ in ids]
        values.append(aggregate([r for case_id in sample for r in cases[case_id]]))
    return statistics.pstdev(values), (percentile(values, 0.025), percentile(values, 0.975))


def verdict(a_name, a_cases, b_name, b_cases, aggregate, min_effect):
    """The beat rule on two runs' per-case results, paired on shared ids."""
    shared = sorted(set(a_cases) & set(b_cases))
    row = {"a": a_name, "b": b_name, "shared_cases": len(shared), "difference": None, "two_se": None}
    if len(shared) < MIN_SHARED:
        row.update(verdict="no_verdict", text=f"no verdict: {len(shared)} shared cases "
                                              f"(at least {MIN_SHARED} needed)")
        return row

    def score(cases, ids):
        return aggregate([r for case_id in ids for r in cases[case_id]])

    difference = score(a_cases, shared) - score(b_cases, shared)
    rng = random.Random(1)
    diffs = []
    for _ in range(BOOTSTRAP):
        sample = [rng.choice(shared) for _ in shared]
        diffs.append(score(a_cases, sample) - score(b_cases, sample))
    two_se = 2 * statistics.pstdev(diffs)
    bar = max(min_effect, two_se)
    row.update(difference=round(difference, 6), two_se=round(two_se, 6))
    points = f"{abs(difference) * 100:.1f} points, 2 SE = {two_se * 100:.1f}"
    if difference > bar:
        row.update(verdict="a_beats_b", text=f"{a_name} beats {b_name} by {points}")
    elif -difference > bar:
        row.update(verdict="b_beats_a", text=f"{b_name} beats {a_name} by {points}")
    elif two_se <= TIE_BAR:
        row.update(verdict="tie", text=f"tie: {a_name} and {b_name} within {points}")
    else:
        row.update(verdict="cannot_separate",
                   text=f"cannot separate {a_name} and {b_name} (2 SE = {two_se * 100:.1f} points): "
                        "add cases")
    return row


# ---------------------------------------------------------------- rows

def text_problem(value, path=""):
    """Where a row breaks the no-text rule, or None (the gateway's rule)."""
    if isinstance(value, str):
        if path not in STRING_FIELDS:
            return f"`{path}` is a string; only identity fields may be"
        return f"`{path}` is longer than {MAX_STRING} characters" if len(value) > MAX_STRING else None
    if isinstance(value, list):
        for i, item in enumerate(value):
            problem = text_problem(item, f"{path}[{i}]")
            if problem:
                return problem
        return None
    if isinstance(value, dict):
        ids = path in ID_MAPS
        for key, item in value.items():
            if ids and not is_case_id(key):
                return f"a key under `{path}` is not a case id"
            if not ids and not is_name(key):
                return f"a key under `{path or 'the row'}` is not a name"
            problem = text_problem(item, f"{path}.{key}" if path else key)
            if problem:
                return problem
    return None


def summed(details):
    out = {}
    for detail in details:
        for key, value in (detail or {}).items():
            out[key] = out.get(key, 0) + value
    return out


def build_row(suite, results, answers, identity, status, extra_repeat, rescored_from=None,
              stop_reason=None):
    """The stored row (scores, ids and counts only) and its readable parts."""
    cases = by_case(results)
    complete = status == "complete" and bool(cases)
    score = suite.aggregate(results) if cases else None
    se, interval = bootstrap(cases, suite.aggregate) if cases else (None, (None, None))
    hard = {}
    for result in results:
        if result["hard_fail"]:
            hard[result["hard_fail"]] = hard.get(result["hard_fail"], 0) + 1
    texts = {}
    for answer in answers:
        texts.setdefault(answer["case_id"], set()).add(answer.get("text"))
    multi = [case_id for case_id, rs in cases.items() if len(rs) > 1]
    differ = sum(1 for case_id in multi if len(texts.get(case_id, ())) > 1)
    seconds = [a["seconds"] for a in answers if isinstance(a.get("seconds"), (int, float))]
    usage = [a.get("usage") or {} for a in answers]
    detail = summed(r["detail"] for r in results)
    row = {
        "run_id": identity["run_id"], "target": identity["target"], "suite": suite.name,
        "suite_version": suite.version, "model_file": identity["model_file"],
        "engine_build": identity["engine_build"], "settings_hash": identity["settings_hash"],
        "settings_revision": identity["settings_revision"], "status": status,
        "score": round(score, 6) if complete else None,
        "se": round(se, 6) if complete and se is not None else None,
        "interval": [round(v, 6) for v in interval] if complete and interval[0] is not None else None,
        "cases": len(cases), "repeats": suite.repeats, "extra_repeat": extra_repeat,
        "concurrency": suite.concurrency, "seed": suite.seed,
        "per_case": {case_id: [round(r["score"], 6) for r in sorted(rs, key=lambda r: r["repeat"])]
                     for case_id, rs in sorted(cases.items())},
        # Nonzero counts only, so a row of a few hundred cases stays small.
        "case_detail": {case_id: [{k: v for k, v in r["detail"].items() if v}
                                  for r in sorted(rs, key=lambda r: r["repeat"])]
                        for case_id, rs in sorted(cases.items())
                        if any(v for r in rs for v in r["detail"].values())},
        # Per repeat {reason: 1} or null: a reason is stored as a name, never as a string.
        "case_fail": {case_id: [{r["hard_fail"]: 1} if r["hard_fail"] else None
                                for r in sorted(rs, key=lambda r: r["repeat"])]
                      for case_id, rs in sorted(cases.items()) if any(r["hard_fail"] for r in rs)},
        "hard_fails": hard,
        "nondeterministic_share": round(differ / len(multi), 4) if multi else None,
        "detail_totals": detail,
        "speed": {"median_s": round(statistics.median(seconds), 3) if seconds else None,
                  "p90_s": round(percentile(seconds, 0.9), 3) if seconds else None,
                  "input_tokens": sum(int(u.get("prompt_tokens") or 0) for u in usage),
                  "output_tokens": sum(int(u.get("completion_tokens") or 0) for u in usage)},
    }
    if "unknown" in detail and detail.get("passed"):
        row["unknown_share"] = round(detail["unknown"] / detail["passed"], 4)
        row["provisional"] = row["unknown_share"] > 0.20
    if rescored_from:
        row["rescored_from"] = rescored_from
    if stop_reason:
        row["stop_reason"] = stop_reason
    return row


def results_of_row(row):
    """Per-(case, repeat) results rebuilt from a stored row."""
    out = []
    details = row.get("case_detail") or {}
    fails = row.get("case_fail") or {}
    for case_id, scores in (row.get("per_case") or {}).items():
        for repeat, score in enumerate(scores):
            detail = (details.get(case_id) or [])
            fail = (fails.get(case_id) or [])
            reason = fail[repeat] if repeat < len(fail) else None
            if isinstance(reason, dict):
                reason = next(iter(reason), None)
            out.append({"case_id": case_id, "repeat": repeat, "score": float(score),
                        "detail": detail[repeat] if repeat < len(detail) and isinstance(detail[repeat], dict) else {},
                        "hard_fail": reason if isinstance(reason, str) else None})
    return out


# ---------------------------------------------------------------- the run

class Evaluator:
    def __init__(self, gw, args, fail_types, paths, environ, out=sys.stdout, err=sys.stderr):
        self.gw = gw
        self.args = args
        self.fail_types = fail_types
        self.paths = paths
        self.environ = environ
        self.out = out
        self.err = err
        self.stopping = False
        self.signal_name = None
        poll = getattr(args, "poll_secs", None)
        # Test runs pass a short poll; every wait scales with it.
        self.poll = 2.0 if poll is None else max(0.01, float(poll))
        self.scale = 1.0 if poll is None else max(0.001, float(poll) / 2.0)
        self.clock = time.monotonic

    # ------------------------------------------------------------ talking

    def log(self, message):
        if not self.args.json:
            print(message, file=self.out, flush=True)

    def say(self, message):
        print(f"sb ai eval: {message}", file=self.err, flush=True)

    def call(self, method, path, body=None, timeout=60, headers=None, doing="talking to the gateway"):
        try:
            return self.gw.call(method, path, body=body, timeout=timeout, headers=headers or {})
        except self.fail_types as error:
            code = getattr(error, "code", None)
            raise EvalError(MACHINE_OFF if code == MACHINE_OFF and not hasattr(error, "status") else FAIL,
                            f"{doing}: {getattr(error, 'message', error)}") from None

    # ------------------------------------------------------------ targets

    def resolve(self, name):
        """One target's identity now: target, lane, host, power, engine
        build, model file, settings hash and revision. GET only."""
        path = f"/v1/models/{urllib.parse.quote(name, safe='/')}/settings"
        view = self.call("GET", path, doing=f"reading the settings of {name}")
        targets = view.get("targets") or []
        exact = [t for t in targets if t.get("target") == name]
        if len(targets) > 1 and len(exact) == 1:
            targets = exact
        if len(targets) != 1:
            names = ", ".join(t.get("target", "?") for t in targets)
            raise EvalError(USAGE, f"{name} reaches {len(targets)} targets ({names}): eval measures "
                                   "targets, not roles; name one as provider/model")
        target = targets[0]
        if "settings_hash" not in target:
            raise EvalError(FAIL, "eval needs an operator or admin key and a gateway that names the "
                                  "answer-settings hash (settings_hash)")
        full = target["target"]
        provider, _, model = full.partition("/")
        engine = ((target.get("load") or {}).get("engine") or {})
        lane = engine.get("lane") or provider
        return dict(self.engine_identity(lane, model), name=name, target=full, model=model, lane=lane,
                    settings_hash=str(target["settings_hash"]),
                    settings_revision=int(target.get("revision") or 0))

    def engine_identity(self, lane, model):
        """The lane's engine build and the model's file from /v1/engines,
        each UNKNOWN when the gateway does not name it (a sleeping machine,
        a gateway that has not listed the engine since it started)."""
        build, model_file = UNKNOWN, UNKNOWN
        try:
            engines = self.gw.call("GET", "/v1/engines", timeout=30, headers={})
        except self.fail_types:
            engines = {}
        for row in engines.get("data") or []:
            if row.get("lane") != lane:
                continue
            build = str(row.get("build") or UNKNOWN)
            for loaded in row.get("models") or []:
                if loaded.get("id") == model:
                    model_file = str(loaded.get("file") or ai_tune.short_hash(json.dumps(loaded.get("args"))))
        return {"engine_build": build, "model_file": model_file}

    def loaded_identity(self, ident):
        """The identity once the model is loaded: settings hash and revision
        read again too, so the row names what actually answered."""
        fresh = self.resolve(ident["name"])
        return {key: fresh[key] for key in ("engine_build", "model_file", "settings_hash",
                                            "settings_revision")}

    def lanes(self):
        return self.call("GET", "/v1/workloads/capacity", timeout=15, doing="reading the lanes").get("data") or []

    def preflight(self, name):
        """Nothing is sent to an engine here, and nothing held or woken."""
        ident = self.resolve(name)
        lanes = self.lanes()
        lane = next((l for l in lanes if l.get("name") == ident["lane"]), None)
        if lane is None:
            raise EvalError(FAIL, f"{ident['target']}: the gateway reports no lane {ident['lane']} "
                                  "(eval runs on a local lane with engine control)")
        if lane.get("experiments") is False:
            raise EvalError(FAIL, f"lane `{ident['lane']}` does not allow experiments "
                                  "(local_executors[].experiments: false); nothing was held or woken")
        host = lane.get("host") or lane.get("name")
        states = {l.get("state") for l in lanes if (l.get("host") or l.get("name")) == host}
        power = "on" if states & set(ON_STATES) else (
            "waking" if "waking" in states else "powering off" if "powering_off" in states else "off")
        ident.update(host=host, power=power, asleep=power != "on")
        if power in ("off", "powering off") and not self.args.wake:
            raise EvalError(MACHINE_OFF, f"{host} is {power}; rerun with --wake to let the gateway wake "
                                         "it, or the eval cannot run")
        return ident

    # ------------------------------------------------------------ hold and lock

    def take_hold_and_lock(self, ident, run_id):
        ttl = min(int(self.budget + 900), MAX_LOCK_S)
        if ident["asleep"]:
            # The lock never wakes anything and refuses a quiet window and a
            # lane with experiments off: on a sleeping machine it comes
            # first, so a refusal leaves the machine asleep.
            self.take_lock(ident, run_id, ttl, None)
            self.take_hold(ident, run_id, ttl, wake=bool(self.args.wake))
            self.wait_awake(ident)
        else:
            self.take_hold(ident, run_id, ttl, wake=False)
            self.take_lock(ident, run_id, ttl, self.hold[1])
        self.last_beat = self.clock()

    def take_hold(self, ident, run_id, ttl, wake):
        # A stop signal waits until the hold's id is recorded, so the
        # release always knows what to let go.
        with ai_tune.stop_signals_deferred():
            hold = self.call("POST", f"/v1/hosts/{urllib.parse.quote(ident['host'])}/holds",
                             body={"ttl_secs": ttl, "reason": f"sb ai eval {run_id}", "wake": wake,
                                   # The gateway ends it by the next quiet window.
                                   "until_quiet_window": True},
                             doing="holding the machine")
            self.hold = (ident["host"], hold.get("id"))

    def take_lock(self, ident, run_id, ttl, hold_id):
        body = {"target": ident["target"], "ttl_secs": ttl, "reason": f"sb ai eval {run_id}"}
        if hold_id:
            body["hold_id"] = hold_id
        with ai_tune.stop_signals_deferred():
            try:
                lock = self.call("POST", "/v1/optimize/locks", body=body,
                                 doing="taking the lane's tuning lock")
            except EvalError as error:
                raise EvalError(FAIL, error.message.replace("taking the lane's tuning lock: ", "")) from None
            self.lock_id = lock.get("id")

    def wait_awake(self, ident):
        deadline = self.clock() + WAKE_TIMEOUT_S * self.scale
        while self.clock() < deadline:
            lane = next((l for l in self.lanes() if l.get("name") == ident["lane"]), None)
            if lane and lane.get("state") in READY_STATES:
                return
            time.sleep(self.poll)
        raise EvalError(FAIL, f"the machine of {ident['target']} did not come up within {WAKE_TIMEOUT_S} s")

    def release(self):
        if self.lock_id:
            try:
                self.call("DELETE", f"/v1/optimize/locks/{self.lock_id}", doing="releasing the lock")
            except EvalError as error:
                self.say(error.message)
            self.lock_id = None
        if self.hold and self.hold[1]:
            host, hold_id = self.hold
            try:
                self.call("DELETE", f"/v1/hosts/{urllib.parse.quote(host)}/holds/{hold_id}",
                          doing="releasing the hold")
            except EvalError as error:
                self.say(error.message)
        self.hold = None

    def heartbeat(self):
        """At a case boundary: renew the lock and read the quiet window."""
        if not self.lock_id or self.clock() - self.last_beat < HEARTBEAT_S * self.scale:
            return
        self.last_beat = self.clock()
        ttl = int(max(60, self.budget - (self.clock() - self.started) + 900))
        try:
            beat = self.gw.call("PATCH", f"/v1/optimize/locks/{self.lock_id}",
                                body={"ttl_secs": min(ttl, MAX_LOCK_S)}, timeout=30, headers={})
        except self.fail_types as error:
            if getattr(error, "status", None) == 404:
                self.lock_id = None
                raise Halt("the tuning lock is gone (it ran out or the gateway restarted)", FAIL,
                           "lock_gone") from None
            self.say(f"renewing the tuning lock: {getattr(error, 'message', error)}")
            return
        window = beat.get("quiet_window")
        if window:
            raise Halt(f"the quiet window {window} began", OK, "quiet_window")

    # ------------------------------------------------------------ requests

    def body_for(self, case, target, seed):
        body = json.loads(json.dumps(case["request"]))
        body["model"] = target
        extra = body.setdefault("switchback", {})
        if not isinstance(extra, dict):
            raise EvalError(USAGE, f"case {case['id']}: `switchback` must be an object")
        # A per-request settings block: the gateway's exact-request cache
        # never answers it, so every repeat reaches the engine.
        extra.setdefault("settings", {})["seed"] = seed
        return body

    def ask(self, case, target, seed):
        body = self.body_for(case, target, seed)
        headers = {LOCK_HEADER: self.lock_id} if self.lock_id else {}
        deadline = self.clock() + PAUSED_WAIT_S * self.scale
        while True:
            started = self.clock()
            try:
                if body.get("stream"):
                    return self.ask_streaming(body, headers, started)
                answer = self.gw.call("POST", "/v1/chat/completions", body=body,
                                      timeout=REQUEST_TIMEOUT_S, headers=headers)
                choice = ((answer or {}).get("choices") or [{}])[0]
                message = choice.get("message") or {}
                return {"text": message.get("content") or "", "finish": choice.get("finish_reason"),
                        "error": None, "usage": (answer or {}).get("usage") or {},
                        "seconds": round(self.clock() - started, 3)}
            except self.fail_types as error:
                status = getattr(error, "status", None)
                paused = status == 503 and "model_paused" in (getattr(error, "reason", ""),
                                                              getattr(error, "error_type", ""))
                if paused and self.clock() < deadline and not self.stopping:
                    # The gateway closed the model's gate (an apply or an
                    # unload runs): ask again later; never a failed answer.
                    time.sleep(max(0.05, 5.0 * self.scale))
                    continue
                if status is None:
                    raise Halt(f"the gateway stopped answering ({getattr(error, 'message', error)})", FAIL,
                               "gateway_gone") from None
                return {"text": "", "finish": None, "usage": {},
                        "error": f"{status}: {getattr(error, 'message', error)}"[:MAX_STRING],
                        "seconds": round(self.clock() - started, 3)}

    def ask_streaming(self, body, headers, started):
        text, finish, usage = [], None, {}
        for _, chunk in self.gw.stream("/v1/chat/completions", body, timeout=REQUEST_TIMEOUT_S,
                                       headers=headers):
            if isinstance(chunk.get("usage"), dict) and chunk["usage"]:
                usage = chunk["usage"]
            for choice in chunk.get("choices") or []:
                delta = choice.get("delta") or choice.get("message") or {}
                text.append(delta.get("content") or "")
                finish = choice.get("finish_reason") or finish
        return {"text": "".join(text), "finish": finish, "error": None, "usage": usage,
                "seconds": round(self.clock() - started, 3)}

    def run_cases(self, ident, suite, cases, repeats):
        """Send every case `repeats` times at the suite's concurrency.
        Answers (case_id, repeat, ...) in completion order, whether the run
        finished, the reason it stopped early and whether one more repeat was
        added for nondeterminism."""
        answers = []
        stop = None
        extra = False
        queue = list(cases)
        current = None     # [case, repeats it gets, next repeat to send]
        sent = {}          # case id -> repeats it gets
        done = {}          # case id -> answers
        texts_differ = []  # per case with all repeats done: whether the texts differ
        pool = ThreadPoolExecutor(max_workers=suite.concurrency)
        running = {}

        def next_job():
            """The next (case, repeat) to send, or None. A case's repeat
            count is decided when its first request goes out."""
            nonlocal current
            if current is None or current[2] == current[1]:
                if not queue:
                    return None
                case = queue.pop(0)
                current = [case, repeats + (1 if extra else 0), 0]
                sent[case["id"]] = current[1]
            job = (current[0], current[2])
            current[2] += 1
            return job

        try:
            while True:
                try:
                    while len(running) < suite.concurrency and stop is None and not self.stopping:
                        if not queue and (current is None or current[2] == current[1]):
                            break
                        try:
                            self.heartbeat()
                            if self.clock() - self.started > self.budget:
                                raise Halt("the budget ran out", OK, "budget")
                        except Halt as halt:
                            stop = halt
                            break
                        case, repeat = next_job()
                        future = pool.submit(self.ask, case, ident["target"], suite.seed)
                        running[future] = (case, repeat)
                    if not running:
                        break
                    finished, _ = wait(list(running), timeout=0.2, return_when=FIRST_COMPLETED)
                except KeyboardInterrupt:
                    # Ctrl-C, SIGTERM, SIGHUP: send nothing more, let the
                    # requests in flight end, then stop.
                    self.stopping = True
                    if not running:
                        break
                    continue
                for future in finished:
                    case, repeat = running.pop(future)
                    try:
                        answer = future.result()
                    except Halt as halt:
                        stop = stop or halt
                        continue
                    answer = dict(answer, case_id=case["id"], repeat=repeat)
                    answers.append(answer)
                    got = done.setdefault(case["id"], [])
                    got.append(answer)
                    if len(got) == sent[case["id"]]:
                        texts_differ.append(len({a["text"] for a in got}) > 1)
                        share = sum(texts_differ) / len(texts_differ)
                        if repeats > 1 and not extra and share > NONDETERMINISTIC:
                            extra = True
                            self.say(f"{share:.0%} of cases answered differently on a repeat: "
                                     "one more repeat for the remaining cases")
                if (stop or self.stopping) and not running:
                    break
        finally:
            pool.shutdown(wait=True)
        if self.stopping:
            raise Interrupted(answers)
        # Only cases with every repeat answered count.
        whole = {case_id for case_id, got in done.items() if len(got) == sent[case_id]}
        kept = [a for a in answers if a["case_id"] in whole]
        finished = stop is None and len(whole) == len(cases)
        return kept, finished, stop, extra

    # ------------------------------------------------------------ one model

    def evaluate(self, ident, suite, folder):
        run_id = "eval_" + uuid.uuid4().hex[:12]
        ident = dict(ident, run_id=run_id)
        cases = suite.cases[: self.args.cases] if self.args.cases else list(suite.cases)
        repeats = self.args.repeats or suite.repeats
        suite.repeats = repeats
        self.lock_id, self.hold = None, None
        self.started = self.clock()
        self.log(f"sb ai eval {ident['target']} on {suite.name} {suite.version}: {len(cases)} cases "
                 f"x {repeats} at concurrency {suite.concurrency} (run {run_id})")
        answers, status, stop, extra, code = [], "failed", None, False, OK
        interrupted = False
        try:
            self.take_hold_and_lock(ident, run_id)
            try:
                self.call("POST", f"/v1/models/{urllib.parse.quote(ident['target'], safe='/')}/load",
                          body={"wake": False}, timeout=LOAD_TIMEOUT_S, doing=f"loading {ident['target']}")
            except EvalError as error:
                raise EvalError(FAIL, error.message) from None
            # The identity read before the wake and the load may not know
            # the model file or the engine build yet (a sleeping machine, a
            # gateway just restarted): the row carries the one read now.
            ident.update(self.loaded_identity(ident))
            unknown = [key.replace("_", " ") for key in ("model_file", "engine_build")
                       if ident[key] == UNKNOWN]
            if unknown:
                raise Halt(f"the gateway does not name the {' or the '.join(unknown)} of the loaded "
                           "model, so a score could never be current; no case was sent",
                           FAIL, "identity_unknown")
            answers, finished, stop, extra = self.run_cases(ident, suite, cases, repeats)
            status = "complete" if finished else "partial"
        except Halt as halt:
            status, stop = "failed", halt
        except Interrupted as stopped:
            answers = [a for a in (stopped.args[0] if stopped.args else [])]
            status, code, interrupted = "partial", INTERRUPTED, True
            stop = Halt(f"interrupted ({self.signal_name or 'Ctrl-C'})", INTERRUPTED, "interrupted")
        except KeyboardInterrupt:
            status, code, interrupted = "partial", INTERRUPTED, True
            stop = Halt(f"interrupted ({self.signal_name or 'Ctrl-C'})", INTERRUPTED, "interrupted")
        finally:
            self.release()
        if stop is not None and code == OK:
            code = stop.code
            where = "before the first case" if status == "failed" else "at a case boundary"
            self.say(f"{ident['target']}: {stop.reason}; stopped {where} ({status})")
        if stop is not None and code == INTERRUPTED:
            self.say(f"{ident['target']}: {stop.reason}; the lock and the hold were let go")
        if not answers and status != "failed":
            return None, code if code != OK else FAIL
        results = self.score_answers(suite, answers)
        row = build_row(suite, results, answers, ident, status, extra,
                        stop_reason=stop.kind if stop is not None else None)
        self.keep(folder, row, ident, answers)
        if not interrupted:
            self.store(row)
        return {"row": row, "results": results}, code

    def score_answers(self, suite, answers):
        cases = {case["id"]: case for case in suite.cases}
        results = []
        for answer in answers:
            case = cases.get(answer["case_id"])
            if case is None:
                continue
            scored = suite.score(case, {"text": answer.get("text") or "", "finish": answer.get("finish"),
                                        "error": answer.get("error"), "usage": answer.get("usage") or {}})
            results.append(dict(scored, case_id=answer["case_id"], repeat=answer["repeat"]))
        return results

    def keep(self, folder, row, ident, answers):
        path = os.path.join(folder, row["run_id"])
        try:
            os.makedirs(path, mode=0o700, exist_ok=True)
            with open(os.path.join(path, "answers.jsonl"), "w", encoding="utf-8") as handle:
                for answer in sorted(answers, key=lambda a: (a["case_id"], a["repeat"])):
                    handle.write(json.dumps({key: answer.get(key) for key in (
                        "case_id", "repeat", "text", "finish", "error", "usage", "seconds")}) + "\n")
            result = dict(row, identity={key: ident.get(key) for key in (
                "target", "model", "lane", "host", "model_file", "engine_build", "settings_hash",
                "settings_revision")}, at_ms=int(time.time() * 1000))
            with open(os.path.join(path, "result.json"), "w", encoding="utf-8") as handle:
                json.dump(result, handle, indent=2)
                handle.write("\n")
        except OSError as error:
            self.say(f"cannot write {path}: {error.strerror}")
        self.log(f"  answers kept in {path}")

    def store(self, row):
        if self.args.no_store:
            return
        problem = text_problem(row)
        if problem or len(json.dumps(row)) > MAX_ROW:
            self.say(f"not stored: the row breaks the no-text rule ({problem or 'over 64 KiB'})")
            return
        try:
            self.call("POST", "/v1/optimize/quality", body=row, doing="storing the score")
        except EvalError as error:
            self.say(error.message)

    # ------------------------------------------------------------ commands

    def trap_stop_signals(self):
        saved = {}
        if threading.current_thread() is not threading.main_thread():
            return saved
        for name in ("SIGTERM", "SIGHUP"):
            signum = getattr(signal, name, None)
            if signum is not None:
                saved[signum] = signal.signal(signum, self._stop_signal)
        return saved

    def _stop_signal(self, signum, _frame):
        self.signal_name = signal.Signals(signum).name
        raise KeyboardInterrupt()

    def run_models(self):
        names = list(self.args.model)
        suite = find_suite(self.args.suite, suite_roots(self.paths, self.environ))
        folder = runs_folder(self.paths, self.environ)
        try:
            self.budget = ai_tune.parse_budget(self.args.budget)
        except ai_tune.TuneError as error:
            raise EvalError(USAGE, error.message) from None
        if self.args.repeats is not None and not 1 <= self.args.repeats <= 10:
            raise EvalError(USAGE, "--repeats must be from 1 to 10")
        if self.args.cases is not None and self.args.cases < 1:
            raise EvalError(USAGE, "--cases must be 1 or more")
        # Every target is checked before anything is held or woken.
        idents = [self.preflight(name) for name in names]
        if len({i["target"] for i in idents}) != len(idents):
            raise EvalError(USAGE, "a target is named twice")
        saved = self.trap_stop_signals()
        runs, code = [], OK
        try:
            for ident in idents:
                if self.stopping:
                    break
                try:
                    run, run_code = self.evaluate(ident, suite, folder)
                except EvalError as error:
                    self.say(f"{ident['target']}: {error.message}")
                    run, run_code = None, error.code
                if run is not None:
                    runs.append(run)
                code = run_code if code == OK else code
                if run_code == INTERRUPTED:
                    break
        finally:
            for signum, handler in saved.items():
                signal.signal(signum, handler)
        verdicts = []
        complete = [r for r in runs if r["row"]["status"] == "complete"]
        for i, a in enumerate(complete):
            for b in complete[i + 1:]:
                verdicts.append(verdict(a["row"]["target"], by_case(a["results"]), b["row"]["target"],
                                        by_case(b["results"]), suite.aggregate, suite.min_effect))
        report = {"suite": suite.name, "suite_version": suite.version,
                  "runs": [r["row"] for r in runs], "verdicts": verdicts}
        self.write_output(report)
        if self.args.json:
            print(json.dumps(report, indent=2), file=self.out)
        else:
            self.print_table(report)
        return code

    def write_output(self, report):
        if not self.args.output:
            return
        path = os.path.abspath(self.args.output)
        try:
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(report, handle, indent=2)
                handle.write("\n")
        except OSError as error:
            raise EvalError(USAGE, f"cannot write {path}: {error.strerror}") from None

    def print_table(self, report):
        print(f"suite {report['suite']} {report['suite_version']}", file=self.out)
        header = (f"{'target':<28} {'score':>6} {'+-2SE':>6} {'status':<9} {'cases':>5} "
                  f"{'nondet':>6} {'med s':>6} {'p90 s':>6}  hard fails")
        print(header, file=self.out)
        for row in report["runs"]:
            score = "-" if row["score"] is None else f"{row['score'] * 100:.1f}"
            two_se = "-" if row["se"] is None else f"{row['se'] * 200:.1f}"
            nondet = "-" if row["nondeterministic_share"] is None else f"{row['nondeterministic_share']:.0%}"
            speed = row.get("speed") or {}
            med = "-" if speed.get("median_s") is None else f"{speed['median_s']:.2f}"
            p90 = "-" if speed.get("p90_s") is None else f"{speed['p90_s']:.2f}"
            fails = ", ".join(f"{k} {v}" for k, v in sorted(row["hard_fails"].items())) or "none"
            stale = " (stale)" if row.get("stale") else ""
            print(f"{row['target'] + stale:<28} {score:>6} {two_se:>6} {row['status']:<9} {row['cases']:>5} "
                  f"{nondet:>6} {med:>6} {p90:>6}  {fails}", file=self.out)
            if row.get("provisional"):
                print(f"  provisional: {row['unknown_share']:.0%} of passing findings are unlabeled; "
                      "extend the labels", file=self.out)
            if row.get("extra_repeat"):
                print("  one more repeat was added: over 10% of cases answered differently", file=self.out)
        for v in report["verdicts"]:
            print(v["text"], file=self.out)

    def current_rows(self, names, suite):
        """Per target: (identity now, newest complete row it may use,
        whether that row is stale and why)."""
        out = []
        for name in names:
            ident = self.resolve(name)
            query = urllib.parse.urlencode({"model": ident["target"], "suite": suite.name, "limit": 200})
            rows = self.call("GET", f"/v1/optimize/quality?{query}", doing="reading stored scores").get("data") or []
            chosen, why = None, []
            for row in rows:
                if row.get("status") != "complete" or row.get("target") != ident["target"]:
                    continue
                # An identity part nobody could name never matches, not
                # even another unknown one.
                reasons = [key for key in ("model_file", "engine_build", "settings_hash")
                           if str(row.get(key)) != str(ident[key])
                           or UNKNOWN in (str(row.get(key)), str(ident[key]))]
                if row.get("suite_version") != suite.version:
                    reasons.append("suite_version")
                if not reasons:
                    chosen, why = row, []
                    break
                if self.args.any and chosen is None:
                    chosen, why = row, reasons
            out.append((ident, chosen, why))
        return out

    def compare(self):
        names = list(self.args.model) + list(self.args.compare or [])
        if len(names) != 2:
            raise EvalError(USAGE, "--compare needs two targets: sb ai eval A --compare B --suite NAME")
        suite = find_suite(self.args.suite, suite_roots(self.paths, self.environ))
        picked = self.current_rows(names, suite)
        rows = []
        for ident, row, why in picked:
            rows.append({"target": ident["target"], "run_id": (row or {}).get("run_id"),
                         "suite_version": (row or {}).get("suite_version"),
                         "score": (row or {}).get("score"), "se": (row or {}).get("se"),
                         "stale": bool(why), "stale_because": why})
        (a_ident, a_row, _), (b_ident, b_row, _) = picked
        missing = [ident["target"] for ident, row, _ in picked if row is None]
        if missing:
            if self.args.any:
                text = f"no verdict: no complete row for {', '.join(missing)}"
            else:
                text = (f"no verdict: no current complete row for {', '.join(missing)} "
                        "(--any allows stale ones)")
            result = {"verdict": "no_verdict", "text": text}
        elif a_row.get("suite_version") != b_row.get("suite_version"):
            result = {"verdict": "no_verdict",
                      "text": f"no verdict: different suite versions ({a_row.get('suite_version')} and "
                              f"{b_row.get('suite_version')})"}
        else:
            result = verdict(a_ident["target"], by_case(results_of_row(a_row)), b_ident["target"],
                             by_case(results_of_row(b_row)), suite.aggregate, suite.min_effect)
        report = {"suite": suite.name, "suite_version": suite.version, "rows": rows, "verdict": result}
        if self.args.json:
            print(json.dumps(report, indent=2), file=self.out)
            return OK
        for row in rows:
            score = "-" if row["score"] is None else f"{row['score'] * 100:.1f}"
            two_se = "" if row["se"] is None else f" +-{row['se'] * 200:.1f}"
            stale = f" (stale: {', '.join(row['stale_because'])})" if row["stale"] else ""
            print(f"{row['target']}: {score}{two_se} run {row['run_id'] or '-'}{stale}", file=self.out)
        print(result["text"], file=self.out)
        return OK

    def rescore(self):
        folder = runs_folder(self.paths, self.environ)
        run_id = self.args.rescore
        if not re.match(r"^[A-Za-z0-9_.-]{1,64}$", run_id or ""):
            raise EvalError(USAGE, f"--rescore {run_id}: not a run id")
        path = os.path.join(folder, run_id)
        old = read_json_file(os.path.join(path, "result.json"))
        answers = read_jsonl(os.path.join(path, "answers.jsonl"))
        suite = find_suite(old.get("suite") or "", suite_roots(self.paths, self.environ))
        suite.repeats = int(old.get("repeats") or suite.repeats)
        ident = dict(old.get("identity") or {})
        for key in ("target", "model_file", "engine_build", "settings_hash", "settings_revision"):
            ident[key] = old.get(key, ident.get(key))
        ident["run_id"] = "eval_" + uuid.uuid4().hex[:12]
        results = self.score_answers(suite, answers)
        row = build_row(suite, results, answers, ident, old.get("status") or "partial",
                        bool(old.get("extra_repeat")), rescored_from=run_id)
        self.keep(folder, row, ident, answers)
        self.store(row)
        report = {"suite": suite.name, "suite_version": suite.version, "runs": [row], "verdicts": []}
        self.write_output(report)
        if self.args.json:
            print(json.dumps(report, indent=2), file=self.out)
        else:
            self.print_table(report)
            print(f"rescored {run_id} as {row['run_id']} (no model call)", file=self.out)
        return OK


def print_list(roots, as_json, out=sys.stdout):
    rows = list_suites(roots)
    if as_json:
        print(json.dumps({"roots": roots, "suites": rows}, indent=2), file=out)
        return OK
    if not rows:
        print(f"no suites in {', '.join(roots)}", file=out)
        return OK
    for row in rows:
        if "error" in row:
            print(f"{row['name']:<24} error: {row['error']}", file=out)
            continue
        roles = ", ".join(row["roles"]) or "-"
        print(f"{row['name']:<24} {row['version']}  {row['kind']:<5} {row['scorer']:<7} "
              f"{row['cases']:>4} cases  roles {roles}", file=out)
    return OK


def add_arguments(parser):
    parser.add_argument("model", nargs="*")
    parser.add_argument("--suite", default="")
    parser.add_argument("--repeats", type=int)
    parser.add_argument("--cases", type=int)
    parser.add_argument("--budget", default="30m")
    parser.add_argument("--wake", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--no-store", action="store_true")
    parser.add_argument("-o", "--output")
    parser.add_argument("--compare", nargs="+")
    parser.add_argument("--any", action="store_true")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--rescore")


def needs_gateway(args):
    """--list never talks to the gateway; --rescore --no-store neither."""
    return not (args.list or (args.rescore and args.no_store))


def run(gw, args, fail_types, paths, environ, poll_secs=None):
    """`sb ai eval` with parsed arguments, a gateway client (None when
    `needs_gateway` says so) and the suites/runs paths from the config."""
    args.poll_secs = poll_secs
    try:
        if args.list:
            return print_list(suite_roots(paths, environ), args.json)
        evaluator = Evaluator(gw, args, fail_types, paths, environ)
        if args.rescore:
            if args.model or args.compare:
                raise EvalError(USAGE, "--rescore takes a run id only")
            return evaluator.rescore()
        if args.compare is not None:
            return evaluator.compare()
        if not args.model:
            raise EvalError(USAGE, "name the target(s): sb ai eval MODEL... --suite NAME")
        return evaluator.run_models()
    except EvalError as error:
        print(f"sb ai: {error.message}", file=sys.stderr)
        return error.code
