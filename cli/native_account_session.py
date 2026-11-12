#!/usr/bin/env python3
"""Let a chat started under one saved Claude account be resumed under another.

Every saved account keeps its own chat history, so Claude Code only finds a chat
under the account that started it. Before `sb claude --account NAME --resume ID`
launches, sb copies that chat into NAME's Claude home:

  projects/<project>/<ID>.jsonl   the chat
  projects/<project>/<ID>/        its side files (subagent chats, saved tool output)
  file-history/<ID>/              its file checkpoints

The newest copy across accounts is the one taken. A copy NAME already holds is
replaced only when the newer one begins with exactly its bytes, which means it
is the same chat carried further. A copy that went its own way is kept, and a
chat still open in a running Claude Code is not copied. The other accounts keep
their copies; nothing is deleted.

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


def begins_with(longer, shorter, length):
    """True when both files hold the same first `length` bytes."""
    with open(longer, "rb") as a, open(shorter, "rb") as b:
        left = length
        while left:
            want = min(CHUNK, left)
            chunk = a.read(want)
            if len(chunk) != want or chunk != b.read(want):
                return False
            left -= want
    return True


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
    # On a tie the launched account's own copy wins, so an equal copy is never rewritten.
    newest = max(found, key=lambda c: (c["mtime"], c["size"], c["account"] == slot))
    if newest["account"] == slot:
        report["skipped"] = "this account already holds the newest copy"
        return report
    report["from"] = newest["account"]
    holder = open_under({n: h for n, h in homes.items() if n != slot}, session)
    if holder:
        raise Refusal("it is still open under %s; close it there first" % holder)
    target = home / "projects" / newest["project"] / (session + ".jsonl")
    if os.path.lexists(target):
        current = owned_file(target)
        if current is None:
            raise Refusal("%s is not a plain file" % target)
        if current.st_size > newest["size"] or not begins_with(newest["path"], target, current.st_size):
            raise Refusal("%s holds a different continuation of it; that copy was kept" % slot)
        if current.st_size == newest["size"]:
            report["skipped"] = "this account already holds the same chat"
            return report
    info = owned_file(newest["path"])
    if info is None:
        raise Refusal("the chat under %s changed while it was being read" % newest["account"])
    copy_file(newest["path"], target, info)
    side = merge_tree(newest["path"].with_suffix(""), target.with_suffix(""))
    side += merge_tree(newest["home"] / "file-history" / session, home / "file-history" / session)
    report.update(copied=True, bytes=info.st_size, side_files=side)
    return report


def summary(report):
    if not report["copied"]:
        return ""
    return "chat %s copied from %s to %s (%.1f MB, %d side files)" % (
        report["session"][:8], report["from"], report["slot"],
        report["bytes"] / 1e6, report["side_files"])


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
