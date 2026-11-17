#!/usr/bin/env python3
"""Estimated token cost of saved Claude and Codex accounts, read from their local logs.

These are subscription accounts, so every amount is an estimate: what the same
tokens would cost at the API list price (`native_account_prices`), labelled
`"basis": "estimated_from_local_logs"`, never a bill.

Reading is incremental. Per file the state keeps identity (device, inode),
size, mtime, the byte offset after the last complete line and a hash of the
bytes just before it. A pass stats every transcript, reads only bytes past the
offset, and leaves an unfinished last line for the next pass. A file whose
inode changed, that shrank, or whose bytes before the offset changed is read
again from the start; a re-read never counts a message twice, because every
message is kept once by its id. On the very first pass, files untouched for
eight days are not read (the window needs today and the last seven days);
`coverage_since` says where the data begins. Messages older than eight days
leave the state, which keeps it bounded.

Copies of one chat. "Chats follow you" copies a chat into every saved
account's Claude home, and a resumed chat continues the copy there. The same
assistant message then sits in several homes. Each message is counted once
across all homes, keyed on message id plus request id (Codex: response id).
A copy that begins with the same bytes as a chat already read is not read
again: the pass checks the first and the last 4 KiB before the shared length
in both files and starts the copy at that length.

Which account paid. A transcript does not say. Birth times cannot tell either:
the copy is an APFS clone, which keeps the original's birth time. The rule:

  * A home is charged to the saved account whose login it records when the
    pass reads it (the default home included). Codex logs belong to the
    account whose home holds them.
  * A message belongs to the home where the collector first saw it. A new line
    appears in the home that wrote it; it reaches other homes only later, by a
    copy. So once the collector runs regularly, first seen is first written.
  * Two homes of different accounts that present the same new message within
    one catch-up (an "epoch": passes until every changed file is read to its
    end) cannot be told apart. That message is unattributed, never guessed and
    never spread. The first pass is one epoch, so chats that already sit in
    several homes start unattributed; that part leaves the 7-day window within
    a week of steady running.

Usage: native_account_cost.py report [--json] [--budget SECONDS]
"""
import argparse
import contextlib
from datetime import date, datetime, timedelta, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import sys
import tempfile
import threading
import time

import native_account_prices as prices

SCHEMA = "switchback/usage-cost@1"
STATE_SCHEMA = "1"
BASIS = "estimated_from_local_logs"
WINDOW_DAYS = 8
CHUNK = 8 << 20
PEEK = 4096
MAX_LINE = 64 << 20
NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
LIMITS_BUDGET = 2.0

TABLES = """
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS files(
  id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL, provider TEXT NOT NULL,
  home TEXT NOT NULL, rel TEXT NOT NULL, dev INTEGER, ino INTEGER, size INTEGER,
  mtime_ns INTEGER, offset INTEGER NOT NULL, tail TEXT, ref_id INTEGER, ref_len INTEGER,
  ctx TEXT);
CREATE INDEX IF NOT EXISTS files_rel ON files(provider, rel);
CREATE TABLE IF NOT EXISTS messages(
  key BLOB PRIMARY KEY, provider TEXT NOT NULL, ts REAL NOT NULL, model TEXT NOT NULL,
  input INTEGER NOT NULL, write_5m INTEGER NOT NULL, write_1h INTEGER NOT NULL,
  cache_read INTEGER NOT NULL, output INTEGER NOT NULL, tokens INTEGER NOT NULL,
  cost_nanos INTEGER, owner TEXT NOT NULL, file_id INTEGER, off INTEGER,
  epoch INTEGER NOT NULL, ambiguous INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS messages_file ON messages(file_id, off);
CREATE INDEX IF NOT EXISTS messages_ts ON messages(ts);
"""


def iso(value):
    return datetime.fromtimestamp(value, timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def parse_ts(value):
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed.timestamp() if parsed.tzinfo else None


def count(value):
    return value if isinstance(value, int) and not isinstance(value, bool) and value > 0 else 0


def key_of(*parts):
    return hashlib.blake2b("\0".join(parts).encode("utf-8", "replace"), digest_size=16).digest()


def local_midnight(day):
    return time.mktime((day.year, day.month, day.day, 0, 0, 0, 0, 0, -1))


def bounds(now):
    """Local calendar day of `now`, the six days before it, and the next midnight."""
    today = datetime.fromtimestamp(now).date()
    return (local_midnight(today), local_midnight(today - timedelta(days=6)),
            local_midnight(today + timedelta(days=1)))


# --- homes -----------------------------------------------------------------

def read_login(path):
    try:
        with open(path, encoding="utf-8") as stream:
            login = json.load(stream).get("oauthAccount")
    except (OSError, ValueError, AttributeError):
        return None
    return login if isinstance(login, dict) else None


def homes(runtime, env=None):
    """Every saved account home: (provider, label, home path, owner, roots).

    The owner is the slot the home is charged to. A Claude home is charged by
    the login it records now; a home with no matching saved login keeps a
    `home:<label>` owner and appears only in `sb costs`.
    """
    from native_account_catalog import account_home
    from native_account_process import Refusal
    env = os.environ if env is None else env
    user_home = Path(env.get("HOME") or Path.home())
    found, bindings = [], {}
    slots = {"claude": [], "codex": []}
    for provider in slots:
        root = Path(runtime) / "config/native-account-slots" / provider
        with contextlib.suppress(OSError):
            for entry in sorted(os.scandir(root), key=lambda e: e.name):
                if not NAME.match(entry.name) or not entry.is_dir(follow_symlinks=False):
                    continue
                try:
                    with open(Path(entry.path) / "slot.json", encoding="utf-8") as stream:
                        meta = json.load(stream)
                    if meta.get("provider") != provider or meta.get("slot") != entry.name:
                        continue
                    home = account_home(runtime, provider, entry.name, meta)
                except (OSError, ValueError, AttributeError, Refusal):
                    continue
                slots[provider].append((entry.name, home, meta))
    default_claude = Path(env.get("SB_CLAUDE_DEFAULT_HOME") or user_home / ".claude")
    for name, _, meta in slots["claude"]:
        identity = meta.get("identity") if isinstance(meta.get("identity"), dict) else {}
        if identity.get("account_uuid"):
            bindings[name] = identity
    claude_homes = [(name, home) for name, home, _ in slots["claude"]]
    if all(Path(home) != default_claude for _, home in claude_homes):
        claude_homes.append(("default", default_claude))
    for label, home in claude_homes:
        record = user_home / ".claude.json" if Path(home) == default_claude else Path(home) / ".claude.json"
        login = read_login(record) or {}
        owner = next((name for name, binding in sorted(bindings.items())
                      if login.get("accountUuid") == binding.get("account_uuid")
                      and (not binding.get("organization_uuid")
                           or login.get("organizationUuid") == binding.get("organization_uuid"))),
                     None)
        found.append(("claude", label, Path(home), owner or "home:" + label, [Path(home) / "projects"]))
    default_codex = Path(env.get("CODEX_HOME") or user_home / ".codex")
    codex_homes = [(name, Path(home), name) for name, home, _ in slots["codex"]]
    if all(home != default_codex for _, home, _ in codex_homes) and (default_codex / "auth.json").is_file():
        codex_homes.append(("default", default_codex, "default"))
    for label, home, owner in codex_homes:
        found.append(("codex", label, home, owner, [home / "sessions", home / "archived_sessions"]))
    return found


def owned(info):
    return stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()


def transcripts(provider, roots):
    """(rel, path, lstat) for every log file, walking real directories only.

    Symlinked project folders point at another folder that is walked anyway;
    following them would count its files twice.
    """
    for root in roots:
        try:
            if not stat.S_ISDIR(os.lstat(root).st_mode):
                continue
        except OSError:
            continue
        # Claude: projects/<project>/<chat>.jsonl and the chat's side folder, whose
        # subagents/ can nest (a subagent's own subagents). Codex: dated folders.
        base = root if provider == "claude" else root.parent
        stack = [root]
        while stack:
            folder = stack.pop()
            with contextlib.suppress(OSError):
                for entry in os.scandir(folder):
                    if entry.name.endswith(".jsonl"):
                        info = entry.stat(follow_symlinks=False)
                        if owned(info):
                            yield os.path.relpath(entry.path, base), entry.path, info
                    elif entry.is_dir(follow_symlinks=False) and entry.name != "memory":
                        stack.append(entry.path)


# --- state -----------------------------------------------------------------

def connect(path):
    path = Path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    db = sqlite3.connect(str(path), timeout=5.0, isolation_level=None)
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("PRAGMA synchronous=NORMAL")
    db.executescript(TABLES)
    version = db.execute("SELECT v FROM meta WHERE k='schema'").fetchone()
    if version is None:
        db.execute("INSERT INTO meta VALUES('schema', ?)", (STATE_SCHEMA,))
    elif version[0] != STATE_SCHEMA:
        raise ValueError("usage cost state schema mismatch")
    with contextlib.suppress(OSError):
        os.chmod(path, 0o600)
    return db


def meta_get(db, key, default=None):
    row = db.execute("SELECT v FROM meta WHERE k=?", (key,)).fetchone()
    return row[0] if row else default


def meta_set(db, key, value):
    db.execute("INSERT INTO meta VALUES(?, ?) ON CONFLICT(k) DO UPDATE SET v=excluded.v", (key, str(value)))


@contextlib.contextmanager
def exclusive(path):
    """One pass at a time; a second caller reads the saved totals instead."""
    lock = Path(str(path) + ".lock")
    lock.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd = os.open(lock, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
            return
        yield True
    finally:
        os.close(fd)


def tail_hash(fd, offset):
    start = max(0, offset - PEEK)
    return hashlib.sha1(os.pread(fd, offset - start, start)).hexdigest()


# --- one pass --------------------------------------------------------------

class Pass:
    def __init__(self, db, now, deadline, clock):
        self.db, self.now, self.deadline, self.clock = db, now, deadline, clock
        self.cutoff = now - WINDOW_DAYS * 86400
        self.epoch = int(meta_get(db, "epoch", "1"))
        self.stats = {"files_seen": 0, "files_read": 0, "bytes_read": 0, "copies_skipped": 0,
                      "bytes_skipped": 0, "messages_new": 0, "bad_lines": 0}

    def late(self):
        return self.deadline is not None and self.clock() > self.deadline

    # Each message is stored once. A known message seen again from another
    # account's home in the same epoch becomes unattributed; a later chunk of
    # the same streamed message (larger usage) replaces the earlier one.
    def record(self, key, provider, ts, model, tokens, cost, owner, file_id, off):
        total = sum(tokens)
        row = self.db.execute("SELECT owner, epoch, ambiguous, output, tokens FROM messages WHERE key=?",
                              (key,)).fetchone()
        if row is None:
            self.db.execute("INSERT INTO messages VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,0)",
                            (key, provider, ts, model, *tokens, total, cost, owner, file_id, off, self.epoch))
            self.stats["messages_new"] += 1
            return
        first_owner, epoch, ambiguous, output, stored = row
        if first_owner != owner and epoch == self.epoch and not ambiguous:
            self.db.execute("UPDATE messages SET ambiguous=1 WHERE key=?", (key,))
        if (tokens[4], total) > (output, stored):
            self.db.execute("UPDATE messages SET model=?, input=?, write_5m=?, write_1h=?, cache_read=?, "
                            "output=?, tokens=?, cost_nanos=? WHERE key=?",
                            (model, *tokens, total, cost, key))

    def claude_line(self, line, off, ctx, owner, file_id):
        if b'"usage"' not in line or b'"type":"assistant"' not in line:
            return
        try:
            entry = json.loads(line)
        except ValueError:
            self.stats["bad_lines"] += 1
            return
        if not isinstance(entry, dict) or entry.get("type") != "assistant":
            return
        message = entry.get("message") if isinstance(entry.get("message"), dict) else {}
        usage = message.get("usage") if isinstance(message.get("usage"), dict) else {}
        ts = parse_ts(entry.get("timestamp"))
        if not usage or ts is None or ts < self.cutoff:
            return
        written = count(usage.get("cache_creation_input_tokens"))
        split = usage.get("cache_creation") if isinstance(usage.get("cache_creation"), dict) else {}
        hour = min(written, count(split.get("ephemeral_1h_input_tokens")))
        tokens = (count(usage.get("input_tokens")), written - hour, hour,
                  count(usage.get("cache_read_input_tokens")), count(usage.get("output_tokens")))
        if not any(tokens):
            return
        message_id, request_id = message.get("id"), entry.get("requestId")
        if isinstance(message_id, str) and isinstance(request_id, str):
            key = key_of("claude", message_id, request_id)
        elif isinstance(entry.get("uuid"), str):
            key = key_of("claude-line", entry["uuid"])
        else:
            self.stats["bad_lines"] += 1
            return
        model = prices.claude_model(message.get("model")) or "unknown"
        cost = prices.claude_cost_nanos(model, tokens[0], tokens[1], tokens[2], tokens[3], tokens[4],
                                        usage.get("speed"), usage.get("inference_geo"))
        self.record(key, "claude", ts, model, tokens, cost, owner, file_id, off)

    def codex_line(self, line, off, ctx, owner, file_id):
        head = line[:256]
        kind = (b'"type":"token_usage_record"' in head and "record" or
                b'"type":"turn_context"' in head and "turn" or
                b'"type":"session_meta"' in head and "meta" or
                b'"type":"token_count"' in head and not ctx.get("records") and "count")
        if not kind:
            return
        try:
            entry = json.loads(line)
        except ValueError:
            self.stats["bad_lines"] += 1
            return
        payload = entry.get("payload") if isinstance(entry, dict) and isinstance(entry.get("payload"), dict) else {}
        if kind == "turn":
            if entry.get("type") == "turn_context" and isinstance(payload.get("model"), str):
                ctx["model"] = payload["model"]
            return
        if kind == "meta":
            if entry.get("type") == "session_meta" and isinstance(payload.get("id"), str):
                ctx["session"] = payload["id"]
            return
        ts = parse_ts(entry.get("timestamp"))
        if kind == "record":
            # One line per API response, with its own id: exact, and the
            # token_count events after it in this file repeat the same usage.
            if entry.get("type") != "token_usage_record" or not isinstance(payload.get("response_id"), str):
                return
            ctx["records"] = True
            usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else {}
            key = key_of("codex", payload["response_id"])
        else:
            info = payload.get("info") if payload.get("type") == "token_count" else None
            if not isinstance(info, dict):
                return
            total = info.get("total_token_usage")
            if total is not None and total == ctx.get("prev"):
                return  # the same call reported again
            ctx["prev"] = total
            usage = info.get("last_token_usage") if isinstance(info.get("last_token_usage"), dict) else {}
            key = key_of("codex-count", ctx.get("session") or str(file_id),
                         json.dumps(total, sort_keys=True) if total is not None else str(entry.get("timestamp")))
        if ts is None or ts < self.cutoff:
            return
        prompt = count(usage.get("input_tokens"))
        cached = min(count(usage.get("cached_input_tokens")), prompt)
        write = min(count(usage.get("cache_write_input_tokens")), prompt - cached)
        tokens = (prompt - cached - write, write, 0, cached, count(usage.get("output_tokens")))
        if not any(tokens):
            return
        model = prices.codex_model(ctx.get("model")) or "unknown"
        cost = prices.codex_cost_nanos(model, tokens[0], tokens[1], tokens[3], tokens[4])
        self.record(key, "codex", ts, model, tokens, cost, owner, file_id, off)

    def mark_shared(self, ref_id, length, owner, depth=0):
        """Messages first read from `ref_id` before `length` also sit in this copy."""
        self.db.execute("UPDATE messages SET ambiguous=1 WHERE file_id=? AND off<? AND epoch=? "
                        "AND owner!=? AND ambiguous=0", (ref_id, length, self.epoch, owner))
        row = self.db.execute("SELECT ref_id, ref_len FROM files WHERE id=?", (ref_id,)).fetchone()
        if row and row[0] is not None and depth < 8:
            self.mark_shared(row[0], min(length, row[1]), owner, depth + 1)

    def shared_prefix(self, fd, info, provider, rel, path, start):
        """Longest verified prefix this file shares with a copy of the same chat already read."""
        best = None
        for ref_id, ref_path, ref_offset, ref_tail in self.db.execute(
                "SELECT id, path, offset, tail FROM files WHERE provider=? AND rel=? AND path!=? "
                "AND offset>? ORDER BY offset DESC LIMIT 8", (provider, rel, path, start)).fetchall():
            length = min(ref_offset, info.st_size)
            if length <= start or not ref_tail:
                continue
            try:
                ref = os.open(ref_path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            except OSError:
                continue
            try:
                ref_info = os.fstat(ref)
                if (not owned(ref_info) or ref_info.st_size < ref_offset
                        or tail_hash(ref, ref_offset) != ref_tail):
                    continue
                low = max(0, length - PEEK)
                mine = os.pread(fd, length - low, low)
                if (mine.endswith(b"\n") and mine == os.pread(ref, length - low, low)
                        and os.pread(fd, min(PEEK, length), 0) == os.pread(ref, min(PEEK, length), 0)):
                    best = (ref_id, length)
                    break
            finally:
                os.close(ref)
        return best

    def read(self, provider, home, owner, rel, path, info, row):
        """Read one file from where the last pass stopped; return False when out of time."""
        db = self.db
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        except OSError:
            return True
        try:
            info = os.fstat(fd)
            if not owned(info):
                return True
            db.execute("BEGIN")
            if row is None:
                db.execute("INSERT INTO files(path, provider, home, rel, offset) VALUES(?,?,?,?,0)",
                           (path, provider, home, rel))
                row = db.execute("SELECT id, dev, ino, size, mtime_ns, offset, tail, ref_id, ref_len, ctx "
                                 "FROM files WHERE path=?", (path,)).fetchone()
            file_id, _, _, _, _, offset, tail, ref_id, ref_len, ctx = row
            ctx = json.loads(ctx) if ctx else {}
            # The bytes before the saved offset must still be the ones read; a
            # rewritten, rotated or shrunk file starts over (ids keep it exact).
            if offset and (info.st_size < offset or tail_hash(fd, offset) != tail):
                offset, ctx, ref_id, ref_len = 0, {}, None, None
            if provider == "claude":
                shared = self.shared_prefix(fd, info, provider, rel, path, offset)
                if shared:
                    self.mark_shared(shared[0], shared[1], owner)
                    self.stats["copies_skipped"] += 1
                    self.stats["bytes_skipped"] += shared[1] - offset
                    offset, ref_id, ref_len = shared[1], shared[0], shared[1]
            handle = self.claude_line if provider == "claude" else self.codex_line
            position, carry, dropping, finished = offset, b"", False, True
            if offset < info.st_size:
                self.stats["files_read"] += 1
            while True:
                if self.late():
                    finished = False
                    break
                chunk = os.pread(fd, CHUNK, position)
                if not chunk:
                    break
                position += len(chunk)
                self.stats["bytes_read"] += len(chunk)
                data = carry + chunk
                base = position - len(data)
                lines = data.split(b"\n")
                carry = lines.pop()
                line_start = base
                for line in lines:
                    if dropping:
                        dropping = False  # end of an oversized line
                    else:
                        handle(line, line_start, ctx, owner, file_id)
                    line_start += len(line) + 1
                offset = line_start
                if len(carry) > MAX_LINE:
                    self.stats["bad_lines"] += 1
                    carry, dropping = b"", True
            db.execute("UPDATE files SET dev=?, ino=?, size=?, mtime_ns=?, offset=?, tail=?, ref_id=?, "
                       "ref_len=?, ctx=? WHERE id=?",
                       (info.st_dev, info.st_ino, info.st_size if finished else -1, info.st_mtime_ns,
                        offset, tail_hash(fd, offset) if offset else None, ref_id, ref_len,
                        json.dumps(ctx) if ctx else None, file_id))
            db.execute("COMMIT")
            return finished
        except BaseException:
            if db.in_transaction:
                db.execute("ROLLBACK")
            raise
        finally:
            os.close(fd)

    def run(self, home_list):
        db = self.db
        if meta_get(db, "coverage_since") is None:
            meta_set(db, "coverage_since", iso(self.cutoff))
        known = {r[0]: r for r in db.execute(
            "SELECT path, id, dev, ino, size, mtime_ns, offset, tail, ref_id, ref_len, ctx FROM files")}
        todo, present, walked = [], set(), True
        db.execute("BEGIN")  # old files are registered in one transaction
        for provider, label, _, owner, roots in home_list:
            for rel, path, info in transcripts(provider, roots):
                self.stats["files_seen"] += 1
                if self.stats["files_seen"] % 512 == 0 and self.late():
                    walked = False
                    break
                present.add(path)
                row = known.get(path)
                if row is None and info.st_mtime < self.cutoff:
                    # Nothing in an untouched file can fall inside the window.
                    self.register_old(provider, label, rel, path, info)
                    continue
                if row is not None and row[2:6] == (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns):
                    continue
                todo.append((provider, label, owner, rel, path, info, row[1:] if row else None))
            if not walked:
                break
        db.execute("COMMIT")
        # Largest copy of a chat first, so the other copies can start where it ends.
        todo.sort(key=lambda t: (t[0], t[3], -t[5].st_size))
        caught_up = walked
        for provider, label, owner, rel, path, info, row in todo:
            if self.late() or not self.read(provider, label, owner, rel, path, info, row):
                caught_up = False
                break
        gone = [p for p in known if p not in present] if walked else []
        db.execute("BEGIN")
        db.executemany("DELETE FROM files WHERE path=?", [(p,) for p in gone])
        db.execute("DELETE FROM messages WHERE ts<?", (self.cutoff,))
        if caught_up:
            meta_set(db, "epoch", self.epoch + 1)
            meta_set(db, "last_complete_at", self.now)
        meta_set(db, "caught_up", int(caught_up))
        db.execute("COMMIT")
        self.stats["caught_up"] = caught_up
        return self.stats

    def register_old(self, provider, label, rel, path, info):
        tail = None
        with contextlib.suppress(OSError):
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            try:
                tail = tail_hash(fd, info.st_size) if info.st_size else None
            finally:
                os.close(fd)
        self.db.execute("INSERT OR REPLACE INTO files(path, provider, home, rel, dev, ino, size, mtime_ns, "
                        "offset, tail) VALUES(?,?,?,?,?,?,?,?,?,?)",
                        (path, provider, label, rel, info.st_dev, info.st_ino, info.st_size,
                         info.st_mtime_ns, info.st_size, tail))


# --- totals ----------------------------------------------------------------

def money(nanos, tokens):
    return {"amount": round(nanos / 1e9, 4), "tokens": int(tokens)}


def summarize(db, home_list, now):
    today, week, tomorrow = bounds(now)
    rows = db.execute(
        "SELECT provider, CASE WHEN ambiguous=1 THEN NULL ELSE owner END, model, cost_nanos IS NOT NULL, "
        "SUM(CASE WHEN ts>=? THEN COALESCE(cost_nanos,0) ELSE 0 END), "
        "SUM(CASE WHEN ts>=? THEN tokens ELSE 0 END), SUM(COALESCE(cost_nanos,0)), SUM(tokens) "
        "FROM messages WHERE ts>=? AND ts<? GROUP BY 1,2,3,4", (today, today, week, tomorrow)).fetchall()
    providers = {}
    for provider, _, _, owner, _ in home_list:
        entry = providers.setdefault(provider, {"provider": provider, "accounts": {}, "models": {},
                                                "unpriced": {}, "total": [0, 0, 0, 0], "unattributed": [0, 0, 0, 0]})
        entry["accounts"].setdefault(owner, [0, 0, 0, 0, 0])
    for provider, owner, model, priced, day_cost, day_tokens, week_cost, week_tokens in rows:
        entry = providers.setdefault(provider, {"provider": provider, "accounts": {}, "models": {},
                                                "unpriced": {}, "total": [0, 0, 0, 0], "unattributed": [0, 0, 0, 0]})
        values = (day_cost, day_tokens, week_cost, week_tokens)
        entry["total"] = [a + b for a, b in zip(entry["total"], values)]
        if owner is None:
            entry["unattributed"] = [a + b for a, b in zip(entry["unattributed"], values)]
        else:
            account = entry["accounts"].setdefault(owner, [0, 0, 0, 0, 0])
            entry["accounts"][owner] = [a + b for a, b in zip(account, values + (0 if priced else week_tokens,))]
        model_total = entry["models"].setdefault(model, [0, 0])
        entry["models"][model] = [model_total[0] + week_cost, model_total[1] + week_tokens]
        if not priced:
            entry["unpriced"][model] = entry["unpriced"].get(model, 0) + week_tokens
    result = []
    for provider in sorted(providers):
        entry = providers[provider]
        t, u = entry["total"], entry["unattributed"]
        result.append({
            "provider": provider,
            "today": money(t[0], t[1]), "last_7_days": money(t[2], t[3]),
            "accounts": [{"account": owner, "today": money(v[0], v[1]), "last_7_days": money(v[2], v[3]),
                          **({"unpriced_tokens": v[4]} if v[4] else {})}
                         for owner, v in sorted(entry["accounts"].items())],
            "unattributed": {"today": money(u[0], u[1]), "last_7_days": money(u[2], u[3]),
                             "reason": "same_message_in_homes_of_different_accounts_within_one_pass"},
            "models": [{"model": m, "last_7_days": money(v[0], v[1])}
                       for m, v in sorted(entry["models"].items(), key=lambda kv: -kv[1][0])],
            "unpriced_models": [{"model": m, "tokens_last_7_days": n} for m, n in sorted(entry["unpriced"].items())],
        })
    return result


def collect(state_path, home_list, now=None, budget=None, clock=time.monotonic):
    """Run one incremental pass (unless another runs) and return totals."""
    now = time.time() if now is None else now
    started = clock()
    deadline = None if budget is None else started + budget
    with exclusive(state_path) as acquired:
        db = connect(state_path)
        try:
            stats = None
            if acquired:
                stats = Pass(db, now, deadline, clock).run(home_list)
                stats["seconds"] = round(clock() - started, 3)
            complete = meta_get(db, "caught_up") == "1"
            sampled = meta_get(db, "last_complete_at")
            return {
                "schema": SCHEMA, "enabled": True, "basis": BASIS, "currency_code": "USD",
                "state": "complete" if complete else "incomplete" if acquired else "busy",
                "sampled_at": iso(float(sampled)) if sampled else None,
                "coverage_since": meta_get(db, "coverage_since"),
                "pass": stats,
                "providers": summarize(db, home_list, float(sampled) if sampled else now),
            }
        finally:
            db.close()


def account_cost(summary, provider, owner):
    if summary.get("state") != "complete" or not summary.get("sampled_at"):
        return None
    for entry in summary["providers"]:
        if entry["provider"] != provider:
            continue
        for account in entry["accounts"]:
            if account["account"] == owner:
                cost = {"currency_code": summary["currency_code"], "basis": summary["basis"],
                        "sampled_at": summary["sampled_at"],
                        "today": account["today"], "last_7_days": account["last_7_days"]}
                if account.get("unpriced_tokens"):
                    cost["unpriced_tokens"] = account["unpriced_tokens"]
                return cost
    return None


# --- entry points ----------------------------------------------------------

def state_path(runtime):
    return Path(runtime) / "state/usage-cost.sqlite"


def status_path(runtime):
    return Path(runtime) / "state/usage-cost-status.json"


def enabled(env=None):
    return (os.environ if env is None else env).get("SB_USAGE_COSTS") == "1"


def write_status(runtime, value):
    path = status_path(runtime)
    with contextlib.suppress(OSError):
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".usage-cost-status.", dir=path.parent)
        with os.fdopen(fd, "w") as out:
            json.dump(value, out)
        os.replace(temporary, path)


def read_status(runtime):
    with contextlib.suppress(OSError, ValueError):
        with open(status_path(runtime), encoding="utf-8") as stream:
            value = json.load(stream)
            return value if isinstance(value, dict) else None
    return None


def attach(result, runtime, limit=LIMITS_BUDGET, env=None):
    """Add a `cost` object to native account rows of an `sb limits` snapshot.

    Runs as its own step with its own time limit. On timeout, an unfinished
    pass or an error, rows carry no `cost`; `sb costs --json` reports why.
    """
    if not enabled(env):
        return result
    box = {}

    def work():
        try:
            box["summary"] = collect(state_path(runtime), homes(runtime, env), budget=max(0.2, limit - 0.4))
        except Exception as error:  # reported through the status file, never raised into limits
            box["error"] = type(error).__name__

    worker = threading.Thread(target=work, name="usage-cost", daemon=True)
    worker.start()
    worker.join(limit)
    summary = box.get("summary")
    state = ("timeout" if worker.is_alive() else "error" if "error" in box else
             summary["state"] if summary else "error")
    attached = 0
    if summary and not worker.is_alive():
        for row in result.get("accounts", []):
            provider = row.get("provider")
            if provider in ("claude", "codex") and row.get("source") == "native/" + provider:
                cost = account_cost(summary, provider, row.get("slot"))
                if cost:
                    row["cost"] = cost
                    attached += 1
    write_status(runtime, {"schema": SCHEMA, "limits_step": {
        "at": iso(time.time()), "state": state, "error": box.get("error"), "accounts_with_cost": attached}})
    return result


def render(summary):
    if not summary.get("enabled"):
        print("Token cost estimates are off. Turn them on: sb costs on")
        return
    print("Estimated cost at API list prices, from local logs (not a bill); state: " + summary["state"])
    for entry in summary["providers"]:
        print(f'{entry["provider"]}: today ~${entry["today"]["amount"]:.2f} ({entry["today"]["tokens"]:,} tokens), '
              f'last 7 days ~${entry["last_7_days"]["amount"]:.2f} ({entry["last_7_days"]["tokens"]:,} tokens)')
        for account in entry["accounts"]:
            print(f'  {account["account"]:<16} today ~${account["today"]["amount"]:.2f}, '
                  f'7 days ~${account["last_7_days"]["amount"]:.2f}')
        u = entry["unattributed"]
        if u["last_7_days"]["tokens"]:
            print(f'  {"unattributed":<16} today ~${u["today"]["amount"]:.2f}, 7 days ~${u["last_7_days"]["amount"]:.2f}'
                  ' (same message in more than one account home)')
        for model in entry["unpriced_models"]:
            print(f'  unpriced model {model["model"]}: {model["tokens_last_7_days"]:,} tokens')
    if summary.get("coverage_since"):
        print("Data since " + summary["coverage_since"] + ".")


def main(argv=None):
    parser = argparse.ArgumentParser(prog="sb costs")
    parser.add_argument("action", choices=("report",))
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--budget", type=float)
    parser.add_argument("--state")
    args = parser.parse_args(argv)
    runtime = os.environ.get("SWITCHBACK_RUNTIME_ROOT", "")
    if not runtime or not os.path.isabs(runtime):
        print("sb costs: an absolute runtime root is required", file=sys.stderr)
        return 2
    if not enabled():
        summary = {"schema": SCHEMA, "enabled": False}
    else:
        try:
            summary = collect(Path(args.state) if args.state else state_path(runtime), homes(runtime),
                              budget=args.budget)
        except (OSError, sqlite3.Error, ValueError) as error:
            summary = {"schema": SCHEMA, "enabled": True, "state": "error", "error": type(error).__name__,
                       "providers": []}
        status = read_status(runtime)
        if status and isinstance(status.get("limits_step"), dict):
            summary["limits_step"] = status["limits_step"]
    if args.json:
        print(json.dumps(summary))
    else:
        render(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
