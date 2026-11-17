#!/usr/bin/env python3
"""Let every saved Claude account read and write one memory per project.

Claude Code keeps its automatic memory in each Claude home, at
<home>/projects/<project>/memory/: a MEMORY.md index and one file per note. A
note saved under one account is unknown to the others. A sync gives each project
one real folder in the Switchback runtime,

  shared/claude-memory/<project>/

and puts a link to it where every signed-in home keeps that project's memory.
Claude Code reads and writes through the link as before, so every account sees
the same notes, however Claude Code was started.

Nothing is deleted:
- a home's own folder steps aside as memory.backup-<time> beside the link, and
  its files are merged into the shared folder (an empty folder is just replaced);
- the same file name with the same bytes is one file; with different bytes the
  newest copy keeps the name and every other version is kept under
  _other-versions/<account>/, which the index does not list;
- MEMORY.md becomes the union of the indexes: one line per linked note and no
  line twice, in the order of the newest index first. An index that loses a line
  to that rule is kept whole under _other-versions/<account>/.

Memory moves only between homes that record a Claude login. A profile that runs
another provider through Claude Code keeps its memory to itself.

A pass that finds nothing new changes nothing. --dry-run reports what a pass
would merge and link, and writes nothing.

usage: native_account_memory.py sync [--project PATH] [--account NAME]... [--dry-run] [--quiet] [--json]
       native_account_memory.py status [--json]
       native_account_memory.py undo
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from native_account_session import LONG, account_homes, copy_file, owned_file, signed_in  # noqa: E402

SHARED = "shared/claude-memory"
INDEX = "MEMORY.md"
OTHER = "_other-versions"
OWN = "_shared"  # the label of the shared folder's own earlier copy under _other-versions
UNMERGED = "memory.unmerged-"  # a home's folder that stepped aside and is not merged yet
BACKUP = "memory.backup-"
LIMIT_LINES, LIMIT_BYTES = 200, 25 * 1024  # what Claude Code loads of MEMORY.md
# A link in an index line: [title](note.md), [title](<note.md>), [title](./note.md "hint").
LINK = re.compile(r"\]\(\s*<?([^()<>\s]+)>?(?:\s+\"[^\"]*\")?\s*\)")


class Refusal(Exception):
    """Nothing was changed; the message says why."""


def sanitize(path):
    """The name Claude Code files a folder under in a home's projects/."""
    return re.sub(r"[^a-zA-Z0-9]", "-", path)


def project_root(folder):
    """The folder Claude Code keeps a project's memory for.

    That is the main checkout of the folder's git repository, so every worktree
    and subfolder shares one memory; outside git it is the folder itself.
    """
    real = os.path.realpath(folder)
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    try:
        result = subprocess.run(
            ["git", "-C", real, "rev-parse", "--path-format=absolute", "--git-common-dir", "--show-toplevel"],
            capture_output=True, text=True, timeout=30, env=env)
    except (OSError, subprocess.SubprocessError):
        return real
    lines = result.stdout.splitlines()
    if result.returncode != 0 or not lines:
        return real
    if os.path.basename(lines[0]) == ".git":
        return os.path.realpath(os.path.dirname(lines[0]))
    return os.path.realpath(lines[1]) if len(lines) > 1 and lines[1] else real


def project_names(folder, homes):
    """The projects/ names a folder's memory is kept under."""
    name = sanitize(project_root(folder))
    if len(name) <= LONG:
        return [name]
    # Claude Code cuts a longer name and adds a hash sb cannot predict; take the
    # names it already made.
    found = set()
    for home in homes.values():
        with contextlib.suppress(OSError):
            found.update(e.name for e in os.scandir(home / "projects") if e.name.startswith(name[:LONG] + "-"))
    return sorted(found)


def all_projects(homes, shared):
    """Every project that has a memory folder or link in a home, or a shared folder."""
    names = set()
    for home in homes.values():
        with contextlib.suppress(OSError):
            for entry in os.scandir(home / "projects"):
                if entry.is_dir(follow_symlinks=False) and os.path.lexists(os.path.join(entry.path, "memory")):
                    names.add(entry.name)
    with contextlib.suppress(OSError):
        names.update(e.name for e in os.scandir(shared) if e.is_dir(follow_symlinks=False) and not e.name.startswith("."))
    return sorted(names)


def files_in(folder):
    """Every file of a memory folder but its index, by path inside it, with its lstat."""
    found = {}
    for top, folders, names in os.walk(folder):
        relative = Path(top).relative_to(folder)
        # _other-versions holds what a sync kept aside; it is never merged again.
        folders[:] = sorted(d for d in folders if not d.startswith(".")
                            and not (relative == Path(".") and d == OTHER))
        for name in sorted(names):
            path = relative / name
            if name.startswith(".") or path == Path(INDEX):
                continue  # dot files include copies a sync is still writing
            info = owned_file(Path(top) / name)
            if info:
                found[path.as_posix()] = (Path(top) / name, info)
    return found


def digest(path):
    sha = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            sha.update(chunk)
    return sha.hexdigest()


def read_text(path):
    with open(path, encoding="utf-8", errors="surrogateescape") as stream:
        return stream.read()


def size(text):
    return len(text.encode("utf-8", "surrogateescape"))


def write_text(path, text):
    """Publish `text` at `path` in one step."""
    fd, temporary = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", errors="surrogateescape") as out:
            out.write(text)
            out.flush()
            os.fsync(out.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)
        raise


def linked_note(line):
    """The note an index line links to, as a path inside the memory folder; None for other lines."""
    for match in LINK.finditer(line):
        ref = match.group(1).split("#")[0]
        if not ref or ":" in ref:
            continue  # a web link or an anchor
        return os.path.normpath(ref[2:] if ref.startswith("./") else ref)
    return None


def merge_indexes(texts):
    """One index from several: the first one's order, then each later one's new lines.

    A note keeps the first line that links to it. A blank line is kept only from
    the first index, and no other line appears twice.
    """
    out, lines_seen, notes_seen = [], set(), set()
    for number, text in enumerate(texts):
        for line in text.splitlines():
            if not line.strip():
                if number == 0:
                    out.append(line)
                continue
            note = linked_note(line)
            if note is not None:
                if note in notes_seen:
                    continue
                notes_seen.add(note)
            elif line in lines_seen:
                continue
            lines_seen.add(line)
            out.append(line)
    text = "\n".join(out).rstrip("\n")
    return text + "\n" if text else ""


def free_name(folder, stem):
    candidate, number = folder / stem, 2
    while os.path.lexists(candidate):
        candidate, number = folder / ("%s-%d" % (stem, number)), number + 1
    return candidate


def keep_version(target, label, relative, version):
    """Keep one other version of a file under _other-versions/<label>/; False when it is already kept."""
    first = target / OTHER / label / relative
    candidate, number = first, 2
    while os.path.lexists(candidate):
        if owned_file(candidate) and digest(candidate) == version["digest"]:
            return False
        candidate, number = first.with_name("%s.%d%s" % (first.stem, number, first.suffix)), number + 1
    copy_file(version["path"], candidate, version["info"])
    return True


def step_aside(memory, place, stamp):
    """Move a home's own memory folder out of the way of the link; None when it was empty."""
    with contextlib.suppress(OSError):
        os.rmdir(memory)  # an empty folder holds nothing to keep
        return None
    aside = free_name(place, UNMERGED + stamp)
    os.rename(memory, aside)
    return aside


def merge(target, sources, report, dry_run):
    """Bring every source folder's files and index lines into the shared folder."""
    versions = {}
    if target.is_dir():
        for relative, (path, info) in files_in(target).items():
            versions.setdefault(relative, []).append({"label": OWN, "path": path, "info": info})
    for label, folder in sources:
        for relative, (path, info) in files_in(folder).items():
            versions.setdefault(relative, []).append({"label": label, "path": path, "info": info})
    for relative, found in sorted(versions.items()):
        if len(found) == 1 and found[0]["label"] == OWN:
            continue
        for version in found:
            version["digest"] = digest(version["path"])
        # The newest copy keeps the name. On a tie the shared copy stays, so a
        # second pass never rewrites what the first one chose.
        newest = max(found, key=lambda v: (v["info"].st_mtime_ns, v["label"] == OWN))
        current = next((v for v in found if v["label"] == OWN), None)
        if len({v["digest"] for v in found}) > 1:
            report["collisions"].append({
                "file": relative, "kept": newest["label"],
                "others": sorted({v["label"] for v in found if v["digest"] != newest["digest"]})})
        elif len(found) > 1:
            report["same"] += 1
        kept = {newest["digest"]}
        for version in found:
            if version["digest"] in kept:
                continue
            kept.add(version["digest"])
            if dry_run or keep_version(target, version["label"], relative, version):
                report["kept_aside"] += 1
        if current is None or current["digest"] != newest["digest"]:
            if not dry_run:
                copy_file(newest["path"], target / relative, newest["info"])
            report["copied"] += 1
    merge_index(target, sources, report, dry_run)


def merge_index(target, sources, report, dry_run):
    base = target / INDEX
    base_info = owned_file(base) if target.is_dir() else None
    others = []
    for label, folder in sources:
        info = owned_file(folder / INDEX)
        if info:
            others.append((label, folder / INDEX, info))
    others.sort(key=lambda one: one[2].st_mtime_ns, reverse=True)  # the newest index leads
    entries = ([(OWN, base, base_info)] if base_info else []) + others
    texts = [read_text(path) for _, path, _ in entries]
    merged, changed = (texts[0] if texts else ""), False
    if others:
        merged = merge_indexes(texts)
        present = set(merged.splitlines())
        for (label, path, info), text in zip(entries, texts):
            if any(line.strip() and line not in present for line in text.splitlines()):
                version = {"path": path, "info": info, "digest": digest(path)}
                if dry_run or keep_version(target, label, INDEX, version):
                    report["kept_aside"] += 1
        changed = base_info is None or texts[0] != merged
        if changed and not dry_run:
            write_text(base, merged)
    report["index"] = {"lines": len(merged.splitlines()), "bytes": size(merged), "changed": changed}


def unmerged_folders(place):
    """Folders an earlier pass moved aside and stopped before merging."""
    try:
        return sorted(Path(e.path) for e in os.scandir(place)
                      if e.name.startswith(UNMERGED) and e.is_dir(follow_symlinks=False))
    except OSError:
        return []


def share_project(name, homes, shared, dry_run, stamp):
    """Link one project's memory in every home to its shared folder, merging what the homes hold."""
    target = shared / name
    report = {"project": name, "linked": [], "moved_aside": [], "already": [], "left": [], "notes": {},
              "copied": 0, "same": 0, "kept_aside": 0, "collisions": [], "index": None}
    if not dry_run:
        target.mkdir(mode=0o700, parents=True, exist_ok=True)
    sources = []
    for account, home in homes.items():
        place = home / "projects" / name
        memory = place / "memory"
        if place.is_symlink():
            # A project folder that links to another project shares that one's
            # memory already; the other project's own pass links it.
            if os.path.lexists(memory):
                report["left"].append({"account": account, "why": "its project folder links to another project"})
            continue
        leftovers = unmerged_folders(place)
        own = None
        if memory.is_symlink():
            if os.path.realpath(memory) != os.path.realpath(target):
                report["left"].append({"account": account, "why": "its memory links somewhere else"})
                continue
            report["already"].append(account)
        elif memory.is_dir():
            notes = len(files_in(memory))
            if notes:
                report["notes"][account] = notes
            if dry_run:
                with os.scandir(memory) as entries:
                    own = memory if any(entries) else None
            else:
                own = step_aside(memory, place, stamp)
            if own is not None:
                report["moved_aside"].append(account)
        elif os.path.lexists(memory):
            report["left"].append({"account": account, "why": "its memory is not a folder"})
            continue
        if account not in report["already"]:
            if dry_run:
                report["linked"].append(account)
            else:
                try:
                    place.mkdir(mode=0o700, parents=True, exist_ok=True)
                    os.symlink(target, memory)
                    report["linked"].append(account)
                except FileExistsError:
                    # Claude Code made a new folder in the moment between the two
                    # steps; the next pass merges it.
                    report["left"].append({"account": account, "why": "a new memory folder appeared; sync again"})
        if own is not None:
            sources.append((account, own))
        for leftover in leftovers:
            notes = len(files_in(leftover))
            if notes:
                report["notes"][account] = report["notes"].get(account, 0) + notes
            sources.append((account, leftover))
    merge(target, sources, report, dry_run)
    if not dry_run:
        for _, folder in sources:
            if folder.name.startswith(UNMERGED):
                os.rename(folder, free_name(folder.parent, BACKUP + folder.name[len(UNMERGED):]))
    return report


def saved_accounts(runtime, wanted=None):
    homes = {name: home for name, home in account_homes(runtime).items()
             if not home.is_symlink() and home.is_dir() and signed_in(name, home)}
    if wanted:
        unknown = sorted(set(wanted) - set(homes))
        if unknown:
            raise Refusal("not a saved Claude account: %s" % ", ".join(unknown))
        homes = {name: home for name, home in homes.items() if name in wanted}
    return homes


def sync(runtime, project=None, accounts=None, dry_run=False, wait=True):
    homes = saved_accounts(runtime, accounts)
    shared = Path(runtime) / SHARED
    names = project_names(project, homes) if project else all_projects(homes, shared)
    result = {"shared": str(shared), "dry_run": dry_run, "accounts": sorted(homes), "projects": []}
    if not names or not homes:
        return result
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    with contextlib.ExitStack() as stack:
        if not dry_run:
            shared.mkdir(mode=0o700, parents=True, exist_ok=True)
            lock = stack.enter_context(open(shared / ".lock", "a"))
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | (0 if wait else fcntl.LOCK_NB))
            except BlockingIOError:
                result["busy"] = True  # another pass is running; it covers this one
                return result
        for name in names:
            result["projects"].append(share_project(name, homes, shared, dry_run, stamp))
    return result


def sync_summary(result, quiet=False):
    projects = result["projects"]
    changed = [p for p in projects if p["linked"] or p["copied"] or p["kept_aside"] or (p["index"] or {}).get("changed")]
    if quiet:
        if not changed:
            return ""
        return "; ".join("shared Claude memory: %s linked for %s (%d file%s merged)" % (
            p["project"], ", ".join(p["linked"]) or "no account", p["copied"], "" if p["copied"] == 1 else "s")
            for p in changed)
    lines = ["Shared Claude memory%s" % (" (dry run: nothing changed)" if result["dry_run"] else ""),
             "  shared folder: %s" % result["shared"],
             "  accounts: %s" % (", ".join(result["accounts"]) or "none signed in")]
    if result.get("busy"):
        lines.append("  another pass is running; nothing done")
        return "\n".join(lines)
    notes = {}
    for p in projects:
        for account, count in p["notes"].items():
            notes[account] = notes.get(account, 0) + count
    already = sum(1 for p in projects if p["already"] and not p["linked"])
    lines.append("  projects: %d (%d already shared by every account)" % (len(projects), already))
    lines.append("  notes in each account's own folders: %s" % (
        ", ".join("%s %d" % (a, notes.get(a, 0)) for a in result["accounts"])))
    verb = "to link" if result["dry_run"] else "linked"
    links = sum(len(p["linked"]) for p in projects)
    aside = sum(len(p["moved_aside"]) for p in projects)
    lines.append("  memory folders %s: %d (%d with notes step aside as memory.backup-<time>)" % (verb, links, aside))
    collisions = [(p["project"], c) for p in projects for c in p["collisions"]]
    lines.append("  files %s: %d; same name and same bytes: %d; same name, other bytes: %d" % (
        "to merge" if result["dry_run"] else "merged", sum(p["copied"] for p in projects),
        sum(p["same"] for p in projects), len(collisions)))
    for project, c in collisions[:20]:
        lines.append("    %s/%s: newest from %s; other version%s from %s kept under %s/" % (
            project, c["file"], c["kept"], "" if len(c["others"]) == 1 else "s", ", ".join(c["others"]), OTHER))
    if len(collisions) > 20:
        lines.append("    and %d more" % (len(collisions) - 20))
    indexed = [p for p in projects if p["index"] and p["index"]["lines"]]
    if indexed:
        top = max(indexed, key=lambda p: (p["index"]["lines"], p["index"]["bytes"]))
        lines.append("  largest index: %s: %d lines, %d bytes (Claude Code loads the first %d lines or %d bytes)" % (
            top["project"], top["index"]["lines"], top["index"]["bytes"], LIMIT_LINES, LIMIT_BYTES))
        over = [p for p in indexed if p["index"]["lines"] > LIMIT_LINES or p["index"]["bytes"] > LIMIT_BYTES]
        if over:
            lines.append("  indexes past that limit: %d (%s)" % (len(over), ", ".join(
                "%s %d lines, %d bytes" % (p["project"], p["index"]["lines"], p["index"]["bytes"]) for p in over[:5])))
    left = [(p["project"], one) for p in projects for one in p["left"]]
    for project, one in left[:10]:
        lines.append("  left alone: %s under %s: %s" % (project, one["account"], one["why"]))
    if len(left) > 10:
        lines.append("  and %d more left alone" % (len(left) - 10))
    return "\n".join(lines)


def status(runtime):
    homes = saved_accounts(runtime)
    shared = Path(runtime) / SHARED
    real_shared = os.path.realpath(shared)
    try:
        projects = sorted(e.name for e in os.scandir(shared) if e.is_dir(follow_symlinks=False) and not e.name.startswith("."))
    except OSError:
        projects = []
    accounts = {}
    for account, home in homes.items():
        counts = {"linked": 0, "own": 0, "elsewhere": 0}
        with contextlib.suppress(OSError):
            for entry in os.scandir(home / "projects"):
                if not entry.is_dir(follow_symlinks=False):
                    continue  # a project folder that links to another is counted there
                memory = os.path.join(entry.path, "memory")
                if os.path.islink(memory):
                    inside = os.path.dirname(os.path.realpath(memory)) == real_shared
                    counts["linked" if inside else "elsewhere"] += 1
                elif os.path.isdir(memory):
                    counts["own"] += 1
        accounts[account] = counts
    return {"shared": str(shared), "projects": len(projects), "accounts": accounts}


def status_summary(report):
    lines = ["  shared folder: %s (%d project%s)" % (
        report["shared"], report["projects"], "" if report["projects"] == 1 else "s")]
    for account, counts in report["accounts"].items():
        text = "  %s: %d linked, %d own folder%s" % (
            account, counts["linked"], counts["own"], "" if counts["own"] == 1 else "s")
        if counts["elsewhere"]:
            text += ", %d linked elsewhere" % counts["elsewhere"]
        lines.append(text)
    return "\n".join(lines)


def undo_steps(runtime):
    """The commands that give each home its own memory folder back, one per link."""
    homes = saved_accounts(runtime)
    real_shared = os.path.realpath(Path(runtime) / SHARED)
    steps = []
    for account, home in homes.items():
        try:
            places = sorted(Path(e.path) for e in os.scandir(home / "projects") if e.is_dir(follow_symlinks=False))
        except OSError:
            continue
        for place in places:
            memory = place / "memory"
            if not memory.is_symlink() or os.path.dirname(os.path.realpath(memory)) != real_shared:
                continue
            backups = sorted(p for p in place.iterdir() if p.name.startswith(BACKUP) and p.is_dir())
            if backups:
                steps.append("rm %s && mv %s %s" % (shlex.quote(str(memory)), shlex.quote(str(backups[-1])),
                                                    shlex.quote(str(memory))))
            else:
                steps.append("rm %s" % shlex.quote(str(memory)))
    return steps


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    one = commands.add_parser("sync")
    one.add_argument("--project")
    one.add_argument("--account", action="append")
    one.add_argument("--dry-run", action="store_true")
    one.add_argument("--quiet", action="store_true")
    one.add_argument("--json", action="store_true")
    commands.add_parser("status").add_argument("--json", action="store_true")
    commands.add_parser("undo")
    args = parser.parse_args(argv)
    runtime = os.environ.get("SWITCHBACK_RUNTIME_ROOT", "")
    try:
        if not runtime or not os.path.isabs(runtime):
            raise Refusal("an absolute SWITCHBACK_RUNTIME_ROOT is required")
        runtime = os.path.realpath(runtime)
        if args.command == "sync":
            if args.project is not None and (not os.path.isabs(args.project) or not os.path.isdir(args.project)):
                raise Refusal("--project takes the full path of a folder")
            result = sync(runtime, args.project, args.account, args.dry_run, wait=not args.quiet)
            text = sync_summary(result, args.quiet)
        elif args.command == "status":
            result = status(runtime)
            text = status_summary(result)
        else:
            steps = undo_steps(runtime)
            print("\n".join(steps) if steps else "No account links to the shared memory folder.")
            return 0
    except (Refusal, OSError) as e:
        print("shared Claude memory: %s" % e, file=sys.stderr)
        return 1
    if getattr(args, "json", False):
        print(json.dumps(result))
    elif text:
        print(text, file=sys.stderr if getattr(args, "quiet", False) else sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
