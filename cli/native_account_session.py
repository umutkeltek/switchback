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
count as said; those lines are then kept beside the new copy as
<ID>.jsonl.dropped-<time>. A copy in which something else was said is kept,
and a chat open in a running Claude Code, under any account, is not copied.
The other accounts keep their copies; nothing is deleted.

`sb claude chats on` asks for more: every interactive launch then does the same
for all chats of the launch folder that another account touched in the last
days, so NAME's resume list and --continue see them as well. That happens only
between homes that record a Claude login. A profile that runs another provider
through Claude Code keeps its chats to itself, and gets none.

A copy shares its data with the original where the filesystem can do that (APFS,
Btrfs, XFS), so it takes no room until one of the two changes. Elsewhere it is
a full copy.

usage: native_account_session.py --slot NAME --session ID [--json]
       native_account_session.py --slot NAME --folder PATH [--days N] [--json]
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
LONG = 200  # Claude Code cuts a longer folder name here and adds a hash of the path


class Refusal(Exception):
    """The chat stays where it is; the message says why, `brief` in a few words."""

    def __init__(self, message, brief=None):
        super().__init__(message)
        self.brief = brief or message


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


def a_copy(account, home, project, path, info):
    return {"account": account, "home": home, "project": project, "path": Path(path),
            "size": info.st_size, "mtime": info.st_mtime_ns}


def project_folders(home):
    try:
        return [e for e in os.scandir(home / "projects") if e.is_dir(follow_symlinks=False)]
    except OSError:
        return []


def copies(homes, session):
    found = []
    for name, home in homes.items():
        for project in project_folders(home):
            path = Path(project.path) / (session + ".jsonl")
            info = owned_file(path)
            if info:
                found.append(a_copy(name, home, project.name, path, info))
    return found


def folder_names(folder):
    """The names Claude Code files a folder's chats under: its path as given and as resolved."""
    names = []
    for path in (folder, os.path.realpath(folder)):
        name = re.sub(r"[^a-zA-Z0-9]", "-", path)
        if name not in names:
            names.append(name)
    return names


def folder_copies(homes, names):
    """Every copy of every chat of that folder, by chat id."""
    chats = {}
    for account, home in homes.items():
        for project in project_folders(home):
            if not any(project.name == name or (len(name) > LONG and project.name.startswith(name[:LONG] + "-"))
                       for name in names):
                continue
            try:
                entries = list(os.scandir(project.path))
            except OSError:
                continue
            for entry in entries:
                session = entry.name[:-len(".jsonl")] if entry.name.endswith(".jsonl") else ""
                info = owned_file(entry.path) if SESSION.fullmatch(session) else None
                if info:
                    chats.setdefault(session, []).append(a_copy(account, home, project.name, entry.path, info))
    return chats


def open_chats(homes):
    """The chats open in a running Claude Code, each with the account that has it open."""
    held = {}
    for name, home in homes.items():
        try:
            records = [e.path for e in os.scandir(home / "sessions") if e.name.endswith(".json")]
        except OSError:
            continue
        for record in records:
            try:
                with open(record, encoding="utf-8") as stream:
                    data = json.load(stream)
                if not isinstance(data, dict) or not isinstance(data.get("sessionId"), str):
                    continue
                pid = data.get("pid")
                if type(pid) is int and pid > 1:
                    os.kill(pid, 0)
                    held.setdefault(data["sessionId"], name)
            except (OSError, ValueError):
                continue  # unreadable record, or its process is gone
    return held


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


def shared_copy(source, temporary):
    """Turn the empty file `temporary` into a copy that shares `source`'s data.

    A chat and its side files can run to gigabytes. A shared copy is its own
    file, but takes no room until one of the two changes. True when the
    filesystem did it; the caller copies the bytes otherwise.
    """
    if sys.platform == "darwin":
        try:
            import ctypes
            clonefile = ctypes.CDLL(None, use_errno=True).clonefile
            clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint32]
            clonefile.restype = ctypes.c_int
        except (ImportError, OSError, AttributeError):
            return False
        os.unlink(temporary)  # clonefile makes the file itself and never replaces one
        if clonefile(os.fsencode(source), os.fsencode(temporary), 1) == 0:  # 1: never through a link
            return True
        os.close(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))
        return False
    if sys.platform.startswith("linux"):
        try:
            import fcntl
            with open(source, "rb") as stream, open(temporary, "wb") as out:
                fcntl.ioctl(out.fileno(), 0x40049409, stream.fileno())  # FICLONE
            return True
        except (ImportError, OSError):
            return False
    return False


def copy_file(source, target, info):
    """Publish a private copy of `source` at `target` in one step, keeping its age."""
    target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix="." + target.name + ".", dir=target.parent)
    try:
        os.close(fd)
        if not shared_copy(source, temporary):
            with open(temporary, "wb") as out, open(source, "rb") as stream:
                shutil.copyfileobj(stream, out, CHUNK)
                out.flush()
                os.fsync(out.fileno())
        os.chmod(temporary, 0o600)
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


def keep_lines(target, start):
    """Save what `target` holds after its first `start` bytes beside it, before those lines go."""
    aside = target.with_name("%s.dropped-%s" % (target.name, time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())))
    fd = os.open(aside, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)  # never over a file that is there
    with os.fdopen(fd, "wb") as out, open(target, "rb") as stream:
        stream.seek(start)
        shutil.copyfileobj(stream, out, CHUNK)
        out.flush()
        os.fsync(out.fileno())
    return aside


def signed_in(name, home):
    """True when this home records a Claude login, which is what makes it a saved account."""
    record = Path.home() / ".claude.json" if name == "default" else home / ".claude.json"
    try:
        with open(record, encoding="utf-8") as stream:
            login = json.load(stream).get("oauthAccount")
    except (OSError, ValueError, AttributeError):
        return False
    return isinstance(login, dict) and any(
        isinstance(login.get(key), str) and login[key] for key in ("accountUuid", "emailAddress"))


def launchable(runtime, slot):
    homes = account_homes(runtime)
    home = homes.get(slot)
    if home is None or home.is_symlink() or not home.is_dir():
        raise Refusal("account %s has no Claude home" % slot)
    return homes


def bring(homes, slot, session, found, held):
    """Bring one chat to `slot` from the account that carried it furthest."""
    home = homes[slot]
    report = {"slot": slot, "session": session, "copied": False}
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
    # A running Claude Code keeps writing to its chat, so neither end may be open:
    # not the copy to take, and not the launched account's own in another window.
    if session in held:
        raise Refusal("it is still open under %s; close it there first" % held[session],
                      "open under %s" % held[session])
    target = home / "projects" / newest["project"] / (session + ".jsonl")
    drop_from = None
    if os.path.lexists(target):
        current = owned_file(target)
        if current is None:
            raise Refusal("%s is not a plain file" % target, "not a plain file")
        shared = shared_length(newest["path"], target)
        if shared == current.st_size == newest["size"]:
            report["skipped"] = "this account already holds the same chat"
            return report
        if shared < current.st_size:
            # This account's copy has lines the newer one lacks. They may go only
            # when nothing was said in them, and they are kept beside the new copy.
            if said_since(target, shared):
                raise Refusal("%s holds a different continuation of it; that copy was kept" % slot,
                              "%s said something else in it" % slot)
            if not said_since(newest["path"], shared):
                report["skipped"] = "this account already holds everything said in it"
                return report
            drop_from = shared
    info = owned_file(newest["path"])
    if info is None:
        raise Refusal("the chat under %s changed while it was being read" % newest["account"], "changed while read")
    aside = None
    if drop_from is not None:
        aside = keep_lines(target, drop_from)
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


def follow(runtime, slot, session):
    homes = launchable(runtime, slot)
    return bring(homes, slot, session, copies(homes, session), open_chats(homes))


def follow_folder(runtime, slot, folder, days):
    """Bring to `slot` the folder's chats that another account touched in the last `days`."""
    homes = launchable(runtime, slot)
    report = {"slot": slot, "folder": folder, "days": days, "brought": [], "left": []}
    # Chats move this way only between saved Claude accounts. A profile that runs
    # another provider through Claude Code must not hand its chats to one, or get any.
    homes = {name: home for name, home in homes.items() if signed_in(name, home)}
    if slot not in homes:
        report["skipped"] = "no Claude login is recorded for %s" % slot
        return report
    chats = folder_copies(homes, folder_names(folder))
    held = open_chats(homes)
    since = (time.time() - days * 86400) * 1e9
    for session in sorted(chats):
        found = chats[session]
        # Only a chat another account touched in these days can hold something new.
        if not any(copy["account"] != slot and copy["mtime"] >= since for copy in found):
            continue
        try:
            one = bring(homes, slot, session, found, held)
        except Refusal as e:
            report["left"].append({"session": session, "why": e.brief})
            continue
        except OSError as e:
            report["left"].append({"session": session, "why": e.strerror or "could not be copied"})
            continue
        if one["copied"]:
            brought = {key: one[key] for key in ("session", "from", "bytes", "side_files")}
            if "kept_as" in one:
                brought["kept_as"] = one["kept_as"]
            report["brought"].append(brought)
    return report


def summary(report):
    if not report["copied"]:
        return ""
    text = "chat %s copied from %s to %s (%.1f MB, %d side files)" % (
        report["session"][:8], report["from"], report["slot"],
        report["bytes"] / 1e6, report["side_files"])
    if report.get("kept_as"):
        text += "; the lines %s's copy had from only being opened are kept as %s" % (report["slot"], report["kept_as"])
    return text


def folder_summary(report):
    parts = []
    brought, left = report["brought"], report["left"]
    if brought:
        sources = {}
        for one in brought:
            sources[one["from"]] = sources.get(one["from"], 0) + 1
        parts.append("%d chat%s of this folder brought to %s (%s; %.1f MB)" % (
            len(brought), "" if len(brought) == 1 else "s", report["slot"],
            ", ".join("%d from %s" % (count, account) for account, count in sorted(sources.items())),
            sum(one["bytes"] for one in brought) / 1e6))
    if left:
        named = ", ".join("%s (%s)" % (one["session"][:8], one["why"]) for one in left[:3])
        if len(left) > 3:
            named += ", and %d more" % (len(left) - 3)
        parts.append("%d left where %s: %s" % (len(left), "it is" if len(left) == 1 else "they are", named))
    return "; ".join(parts)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--slot", required=True)
    what = parser.add_mutually_exclusive_group(required=True)
    what.add_argument("--session")
    what.add_argument("--folder")
    parser.add_argument("--days", type=int, default=7)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    runtime = os.environ.get("SWITCHBACK_RUNTIME_ROOT", "")
    try:
        if not runtime or not os.path.isabs(runtime):
            raise Refusal("an absolute SWITCHBACK_RUNTIME_ROOT is required")
        if not args.slot or "/" in args.slot or args.slot.startswith("."):
            raise Refusal("invalid account name")
        if args.folder is None:
            if not SESSION.fullmatch(args.session):
                raise Refusal("not a chat id")
            report = follow(os.path.realpath(runtime), args.slot, args.session)
            text = summary(report)
        else:
            if not os.path.isabs(args.folder):
                raise Refusal("the folder must be a full path")
            if not 1 <= args.days <= 3650:
                raise Refusal("--days takes 1 to 3650")
            report = follow_folder(os.path.realpath(runtime), args.slot, args.folder, args.days)
            text = folder_summary(report)
    except (Refusal, OSError) as e:
        print("%s not copied to %s: %s" % ("chat" if args.folder is None else "chats", args.slot, e), file=sys.stderr)
        return 1
    if args.json:
        print(json.dumps(report))
    elif text:
        print(text, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
