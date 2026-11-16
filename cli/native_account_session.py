#!/usr/bin/env python3
"""Let a chat started under one saved Claude account be resumed under another.

Every saved account keeps its own chat history, so Claude Code only finds a chat
under the account that started it. Before `sb claude --account NAME --resume ID`
launches, sb copies that chat into NAME's Claude home:

  projects/<project>/<ID>.jsonl   the chat
  projects/<project>/<ID>/        its side files (subagent chats, saved tool output)
  file-history/<ID>/              its file checkpoints

The copy taken is the one that holds the last thing said, by the time stamped
on that turn. File times cannot tell: Claude Code rewrites a chat's last lines
whenever the chat is opened, so a copy that was only looked at would pass for
the newest one.

A copy NAME already holds is replaced only by the same chat carried further:
the newer copy must begin with everything said in NAME's copy. Lines Claude
Code added to NAME's copy when the chat was opened and nothing was asked do not
count as said; that copy is then kept beside the new one as
<ID>.jsonl.replaced-<time>. A copy in which something else was said is kept,
and a chat still open in a running Claude Code is not copied. The other
accounts keep their copies; nothing is deleted.

usage: native_account_session.py --slot NAME --session ID [--json]
"""
import argparse
import contextlib
import json
import os
import re
import shutil
import stat
import sys
import tempfile
import time
from pathlib import Path

SESSION = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
CHUNK = 1 << 20


class Refusal(Exception):
    """The chat stays where it is; the message says why."""


def default_home():
    override = os.environ.get("SB_CLAUDE_DEFAULT_HOME")
    return Path(override) if override else Path.home() / ".claude"


def account_homes(runtime):
    """Every Claude home sb can launch, by account name (the `sb claude` account set)."""
    homes = {"default": default_home()}
    with contextlib.suppress(OSError):
        for entry in sorted(os.scandir(Path(runtime) / "config/claude"), key=lambda e: e.name):
            # `_providers` is transport profile state, never an account.
            if (entry.name not in ("default", "_providers") and not entry.name.startswith(".")
                    and entry.is_dir(follow_symlinks=False)):
                homes[entry.name] = Path(entry.path)
    return homes


def owned_file(path):
    """lstat of a regular file this user owns; None for anything else."""
    try:
        info = os.lstat(path)
    except OSError:
        return None
    return info if stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() else None


def copies(homes, session):
    found = []
    for name, home in homes.items():
        try:
            projects = [e for e in os.scandir(home / "projects") if e.is_dir(follow_symlinks=False)]
        except OSError:
            continue
        for project in projects:
            path = Path(project.path) / (session + ".jsonl")
            info = owned_file(path)
            if info:
                found.append({"account": name, "home": home, "project": project.name, "path": path,
                              "size": info.st_size, "mtime": info.st_mtime_ns})
    return found


def open_under(homes, session):
    """The account whose running Claude Code has this chat open, if any."""
    for name, home in homes.items():
        try:
            records = [e.path for e in os.scandir(home / "sessions") if e.name.endswith(".json")]
        except OSError:
            continue
        for record in records:
            try:
                with open(record, encoding="utf-8") as stream:
                    data = json.load(stream)
                if not isinstance(data, dict) or data.get("sessionId") != session:
                    continue
                pid = data.get("pid")
                if type(pid) is int and pid > 1:
                    os.kill(pid, 0)
                    return name
            except (OSError, ValueError):
                continue  # unreadable record, or its process is gone
    return None


def said(line):
    """When one line of a chat was said, as the time stamped on it.

    A chat holds what the person and the model said, and Claude Code's own
    housekeeping: titles, modes, hook output, and the stand-in reply it writes
    when a chat is opened and nothing is asked. Housekeeping gives "". A line
    that cannot be read, or a turn without a time, gives None; callers count
    that as said, so it is never dropped.
    """
    try:
        record = json.loads(line)
    except ValueError:
        return None
    if not isinstance(record, dict) or not isinstance(record.get("type"), str):
        return None
    message = record.get("message")
    stand_in = isinstance(message, dict) and message.get("model") == "<synthetic>"
    if record["type"] == "user" or (record["type"] == "assistant" and not stand_in):
        stamp = record.get("timestamp")
        return stamp if isinstance(stamp, str) and stamp else None
    return ""


def last_said(path):
    """The time of the last thing said in a chat; "" when no turn in it carries one."""
    with open(path, "rb") as stream:
        end = stream.seek(0, os.SEEK_END)
        rest = b""
        while end:
            start = max(0, end - CHUNK)
            stream.seek(start)
            lines = (stream.read(end - start) + rest).split(b"\n")
            # The first piece is the end of a line that began before this block.
            rest = lines.pop(0) if start else b""
            for line in reversed(lines):
                # Only a turn can answer, and a chat's other lines can be very large.
                if b'"user"' in line or b'"assistant"' in line:
                    stamp = said(line)
                    if stamp:
                        return stamp
            end = start
    return ""


def said_since(path, start):
    """True when something was said in a chat after its first `start` bytes."""
    with open(path, "rb") as stream:
        stream.seek(start)
        return any(line.strip() and said(line) != "" for line in stream)


def shared_length(one, other):
    """How many bytes two copies of a chat begin with in common.

    The whole of the shorter copy when the longer one begins with it; otherwise
    the end of the last line both hold, so what follows is whole lines.
    """
    with open(one, "rb") as a, open(other, "rb") as b:
        same = whole_lines = 0
        while True:
            left, right = a.read(CHUNK), b.read(CHUNK)
            count = min(len(left), len(right))
            if left[:count] == right[:count]:
                if count < CHUNK:
                    return same + count
                cut = left.rfind(b"\n")
                if cut >= 0:
                    whole_lines = same + cut + 1
                same += count
                continue
            first = next(i for i in range(count) if left[i] != right[i])
            cut = left.rfind(b"\n", 0, first)
            return same + cut + 1 if cut >= 0 else whole_lines


def copy_file(source, target, info):
    """Publish a private copy of `source` at `target` in one step, keeping its age."""
    target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix="." + target.name + ".", dir=target.parent)
    try:
        with os.fdopen(fd, "wb") as out, open(source, "rb") as stream:
            os.fchmod(out.fileno(), 0o600)
            shutil.copyfileobj(stream, out, CHUNK)
            out.flush()
            os.fsync(out.fileno())
        os.utime(temporary, ns=(info.st_atime_ns, info.st_mtime_ns))
        os.replace(temporary, target)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)
        raise


def merge_tree(source, target):
    """Copy the files `target` lacks or holds an older version of. Removes nothing."""
    if source.is_symlink() or not source.is_dir() or target.is_symlink():
        return 0
    copied = 0
    for folder, _, names in os.walk(source):
        relative = Path(folder).relative_to(source)
        for name in names:
            info = owned_file(Path(folder) / name)
            if info is None:
                continue
            destination = target / relative / name
            if os.path.lexists(destination):
                current = owned_file(destination)
                if current is None or current.st_mtime_ns >= info.st_mtime_ns:
                    continue
            copy_file(Path(folder) / name, destination, info)
            copied += 1
    return copied


def follow(runtime, slot, session):
    homes = account_homes(runtime)
    home = homes.get(slot)
    if home is None or home.is_symlink() or not home.is_dir():
        raise Refusal("account %s has no Claude home" % slot)
    report = {"slot": slot, "session": session, "copied": False}
    found = copies(homes, session)
    if not found:
        report["skipped"] = "no account holds this chat"
        return report
    # The copy to take holds the last thing said. Among copies that all hold it,
    # the launched account's own wins, so an equal copy is never rewritten. A chat
    # whose turns carry no time falls back to file times.
    for copy in found:
        copy["said"] = last_said(copy["path"])
    latest = max(copy["said"] for copy in found)
    newest = max((copy for copy in found if copy["said"] == latest),
                 key=lambda c: (bool(latest) and c["account"] == slot, c["mtime"], c["size"], c["account"] == slot))
    if newest["account"] == slot:
        report["skipped"] = "this account already holds the newest copy"
        return report
    report["from"] = newest["account"]
    holder = open_under({n: h for n, h in homes.items() if n != slot}, session)
    if holder:
        raise Refusal("it is still open under %s; close it there first" % holder)
    target = home / "projects" / newest["project"] / (session + ".jsonl")
    aside = None
    if os.path.lexists(target):
        current = owned_file(target)
        if current is None:
            raise Refusal("%s is not a plain file" % target)
        shared = shared_length(newest["path"], target)
        if shared == current.st_size == newest["size"]:
            report["skipped"] = "this account already holds the same chat"
            return report
        if shared < current.st_size:
            # This account's copy has lines the newer one lacks. They may go only
            # when nothing was said in them, and the copy holding them is kept.
            if said_since(target, shared):
                raise Refusal("%s holds a different continuation of it; that copy was kept" % slot)
            if not said_since(newest["path"], shared):
                report["skipped"] = "this account already holds everything said in it"
                return report
            aside = target.with_name("%s.replaced-%s" % (target.name, time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())))
    info = owned_file(newest["path"])
    if info is None:
        raise Refusal("the chat under %s changed while it was being read" % newest["account"])
    if aside:
        os.link(target, aside)  # fails if that name is taken; the old copy is never overwritten
        report["kept_as"] = aside.name
    try:
        copy_file(newest["path"], target, info)
    except BaseException:
        if aside:
            os.unlink(aside)
        raise
    side = merge_tree(newest["path"].with_suffix(""), target.with_suffix(""))
    side += merge_tree(newest["home"] / "file-history" / session, home / "file-history" / session)
    report.update(copied=True, bytes=info.st_size, side_files=side)
    return report


def summary(report):
    if not report["copied"]:
        return ""
    text = "chat %s copied from %s to %s (%.1f MB, %d side files)" % (
        report["session"][:8], report["from"], report["slot"],
        report["bytes"] / 1e6, report["side_files"])
    if report.get("kept_as"):
        text += "; the copy %s had only opened is kept as %s" % (report["slot"], report["kept_as"])
    return text


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--slot", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    runtime = os.environ.get("SWITCHBACK_RUNTIME_ROOT", "")
    try:
        if not runtime or not os.path.isabs(runtime):
            raise Refusal("an absolute SWITCHBACK_RUNTIME_ROOT is required")
        if not args.slot or "/" in args.slot or args.slot.startswith("."):
            raise Refusal("invalid account name")
        if not SESSION.fullmatch(args.session):
            raise Refusal("not a chat id")
        report = follow(os.path.realpath(runtime), args.slot, args.session)
    except (Refusal, OSError) as e:
        print("chat not copied to %s: %s" % (args.slot, e), file=sys.stderr)
        return 1
    if args.json:
        print(json.dumps(report))
    else:
        text = summary(report)
        if text:
            print(text, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
