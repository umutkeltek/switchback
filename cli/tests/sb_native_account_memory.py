#!/usr/bin/env python3
"""Every saved Claude account reads and writes one memory folder per project."""
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
HELPER = CLI / "native_account_memory.py"
PROJECT = "-work-repo"
INDEX = "MEMORY.md"
OTHER = "_other-versions"
NOW = 1_800_000_000


def filed_under(folder):
    """The name Claude Code files a folder's memory under."""
    return re.sub(r"[^a-zA-Z0-9]", "-", os.path.realpath(folder))


def snapshot(root, skip=()):
    """Everything under `root`: each entry's kind, bytes or link, age and mode."""
    seen = []
    for top, folders, names in os.walk(root):
        folders.sort()
        for name in sorted(folders + names):
            path = Path(top) / name
            relative = str(path.relative_to(root))
            if relative in skip:
                continue
            info = os.lstat(path)
            if stat.S_ISLNK(info.st_mode):
                seen.append((relative, "link", os.readlink(path)))
            elif stat.S_ISDIR(info.st_mode):
                seen.append((relative, "folder", info.st_mode & 0o777))
            else:
                seen.append((relative, "file", path.read_bytes(), info.st_mtime_ns, info.st_mode & 0o777))
    return seen


class Homes(unittest.TestCase):
    """Three saved accounts (default, seat1, seat2) and one provider profile, in a temporary home."""

    def setUp(self):
        # sb refuses an account home that others may write to.
        self.addCleanup(os.umask, os.umask(0o077))
        self.temp = tempfile.TemporaryDirectory(prefix="sb-shared-memory-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.runtime = self.home / ".switchback"
        self.profiles = self.runtime / "config/claude"
        self.default = self.home / ".claude"
        self.seat1 = self.profiles / "seat1"
        self.seat2 = self.profiles / "seat2"
        self.lane = self.profiles / "lane"  # runs another provider through Claude Code: no Claude login
        for path in (self.default, self.seat1, self.seat2, self.lane, self.profiles / "_providers/x"):
            path.mkdir(parents=True)
        for record, who in ((self.home, "d"), (self.seat1, "seat1"), (self.seat2, "seat2")):
            (record / ".claude.json").write_text(
                json.dumps({"oauthAccount": {"emailAddress": who + "@example.invalid"}}))
        (self.runtime / "manifest.json").write_text("{}")
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("SB_", "SWITCHBACK_", "CLAUDE_", "ANTHROPIC_", "GIT_"))}
        self.env.update(HOME=str(self.home), SWITCHBACK_RUNTIME_ROOT=str(self.runtime))

    def note(self, home, name, body, age=0, project=PROJECT):
        """Write a memory file under `home`; a larger `age` makes it older."""
        path = home / "projects" / project / "memory" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
        os.utime(path, (NOW - age, NOW - age))
        return path

    def memory(self, home, project=PROJECT):
        return home / "projects" / project / "memory"

    def shared(self, project=PROJECT):
        return self.runtime / "shared/claude-memory" / project

    def sync(self, *args):
        result = subprocess.run([sys.executable, "-B", str(HELPER), "sync", "--json", *args],
                                env=self.env, capture_output=True, text=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        return report, {one["project"]: one for one in report["projects"]}

    def everything(self):
        """Notes that meet each case at once: new, identical, different, indexed, and a folderless account."""
        self.note(self.default, "a.md", "note a\n", age=50)
        self.note(self.default, "x.md", "older x\n", age=100)
        self.note(self.default, "same.md", "same words\n", age=40)
        self.note(self.default, INDEX, "# Memory\n- [A](a.md) — a\n- [X](x.md) — first words\n", age=50)
        self.note(self.seat1, "x.md", "newer x\n", age=5)
        self.note(self.seat1, "same.md", "same words\n", age=3)
        self.note(self.seat1, INDEX, "# Memory\n- [X](x.md) — later words\n- [S](same.md) — s\n", age=3)
        self.note(self.seat2, "solo.md", "only seat2\n", project="-other-repo")


class ShareTests(Homes):
    def test_notes_of_every_account_meet_in_one_folder(self):
        self.note(self.default, "a.md", "note a\n", age=50)
        self.note(self.default, INDEX, "- [A](a.md) — a\n", age=50)
        self.note(self.seat1, "topic/b.md", "note b\n", age=10)
        self.note(self.seat1, INDEX, "- [B](topic/b.md) — b\n", age=10)
        report, projects = self.sync()
        self.assertEqual(report["accounts"], ["default", "seat1", "seat2"])
        one = projects[PROJECT]
        self.assertEqual((one["collisions"], one["notes"]), ([], {"default": 1, "seat1": 1}))
        shared = self.shared()
        self.assertEqual(sorted(str(p.relative_to(shared)) for p in shared.rglob("*") if p.is_file()),
                         [INDEX, "a.md", "topic/b.md"])
        self.assertEqual((shared / "a.md").stat().st_mtime_ns, (NOW - 50) * 10**9)
        self.assertEqual((shared / "a.md").stat().st_mode & 0o777, 0o600)
        # The newest index leads.
        self.assertEqual((shared / INDEX).read_text(), "- [B](topic/b.md) — b\n- [A](a.md) — a\n")
        for home in (self.default, self.seat1, self.seat2):
            memory = self.memory(home)
            self.assertTrue(memory.is_symlink(), home)
            self.assertEqual(os.path.realpath(memory), str(shared))
            self.assertEqual((memory / "topic/b.md").read_text(), "note b\n")
        # Each account's own folder stays beside the link, as it was.
        backup, = (self.default / "projects" / PROJECT).glob("memory.backup-*")
        self.assertEqual(sorted(p.name for p in backup.iterdir()), [INDEX, "a.md"])
        self.assertEqual((backup / "a.md").read_text(), "note a\n")
        self.assertFalse((shared / OTHER).exists())

    def test_same_file_with_same_bytes_is_one_file(self):
        self.note(self.default, "same.md", "same words\n", age=30)
        self.note(self.seat1, "same.md", "same words\n", age=5)
        _, projects = self.sync()
        one = projects[PROJECT]
        self.assertEqual((one["same"], one["collisions"], one["kept_aside"]), (1, [], 0))
        self.assertEqual(sorted(p.name for p in self.shared().iterdir()), ["same.md"])
        self.assertEqual((self.shared() / "same.md").read_text(), "same words\n")

    def test_same_file_with_other_bytes_keeps_the_newest_and_every_other_version(self):
        self.note(self.default, "x.md", "middle\n", age=100)
        self.note(self.seat1, "x.md", "newest\n", age=0)
        self.note(self.seat2, "x.md", "oldest\n", age=200)
        self.note(self.seat1, INDEX, "- [X](x.md) — x\n")
        _, projects = self.sync()
        one = projects[PROJECT]
        self.assertEqual(one["collisions"], [{"file": "x.md", "kept": "seat1", "others": ["default", "seat2"]}])
        self.assertEqual(one["kept_aside"], 2)
        shared = self.shared()
        self.assertEqual((shared / "x.md").read_text(), "newest\n")
        self.assertEqual((shared / OTHER / "default/x.md").read_text(), "middle\n")
        self.assertEqual((shared / OTHER / "seat2/x.md").read_text(), "oldest\n")
        self.assertEqual((shared / INDEX).read_text(), "- [X](x.md) — x\n")

    def test_index_is_the_union_with_one_line_per_note_and_no_line_twice(self):
        older = "# Memory\n\n- [A](a.md) — first words\n- [C](c.md) — c\n"
        oldest = "# Memory\n- [B](b.md) — b again\n- [C](c.md) — c\n"
        self.note(self.default, INDEX, older, age=60)
        self.note(self.seat1, INDEX, "# Memory\n\n- [A](a.md) — later words\n- [B](./b.md) — b\n"
                  "See the [docs](https://example.invalid/a.md).\n", age=5)
        self.note(self.seat2, INDEX, oldest, age=90)
        _, projects = self.sync()
        merged = (self.shared() / INDEX).read_text()
        self.assertEqual(merged, "# Memory\n\n- [A](a.md) — later words\n- [B](./b.md) — b\n"
                                 "See the [docs](https://example.invalid/a.md).\n- [C](c.md) — c\n")
        lines = [line for line in merged.splitlines() if line.strip()]
        self.assertEqual(len(lines), len(set(lines)))
        # An index that lost a line to the one-line-per-note rule is kept whole.
        self.assertEqual((self.shared() / OTHER / "default" / INDEX).read_text(), older)
        self.assertEqual((self.shared() / OTHER / "seat2" / INDEX).read_text(), oldest)
        self.assertFalse((self.shared() / OTHER / "seat1").exists())
        self.assertEqual(projects[PROJECT]["index"],
                         {"lines": len(merged.splitlines()), "bytes": len(merged.encode()), "changed": True})

    def test_account_without_a_folder_gets_a_link_and_an_empty_folder_is_replaced(self):
        self.note(self.default, "a.md", "note a\n")
        self.memory(self.seat1).mkdir(parents=True)
        self.assertFalse((self.seat2 / "projects").exists())
        _, projects = self.sync()
        self.assertEqual(sorted(projects[PROJECT]["linked"]), ["default", "seat1", "seat2"])
        self.assertEqual(projects[PROJECT]["moved_aside"], ["default"])
        memory = self.memory(self.seat2)
        self.assertEqual(os.readlink(memory), str(self.shared()))
        self.assertEqual((memory / "a.md").read_text(), "note a\n")
        self.assertEqual(sorted(p.name for p in (self.seat1 / "projects" / PROJECT).iterdir()), ["memory"])
        # A note saved under one account is the same file under every other.
        (memory / "b.md").write_text("from seat2\n")
        self.assertEqual((self.memory(self.default) / "b.md").read_text(), "from seat2\n")
        self.assertEqual((self.memory(self.seat1) / "b.md").stat().st_ino, (memory / "b.md").stat().st_ino)

    def test_second_pass_changes_nothing(self):
        self.everything()
        self.sync()
        before = snapshot(self.home)
        _, projects = self.sync()
        self.assertEqual(snapshot(self.home), before)
        for one in projects.values():
            self.assertEqual((one["linked"], one["moved_aside"], one["copied"], one["kept_aside"],
                              one["collisions"], one["index"]["changed"]), ([], [], 0, 0, [], False))
            self.assertEqual(one["already"], ["default", "seat1", "seat2"])

    def test_dry_run_changes_nothing_and_foretells_the_pass(self):
        self.everything()
        before = snapshot(self.home)
        report, projects = self.sync("--dry-run")
        self.assertEqual(snapshot(self.home), before)
        self.assertFalse((self.runtime / "shared").exists())
        self.assertTrue(report["dry_run"])
        keys = ("linked", "moved_aside", "copied", "same", "kept_aside", "collisions", "notes", "index")
        foretold = {name: {k: one[k] for k in keys} for name, one in projects.items()}
        self.assertEqual(foretold[PROJECT]["collisions"], [{"file": "x.md", "kept": "seat1", "others": ["default"]}])
        text = subprocess.run([sys.executable, "-B", str(HELPER), "sync", "--dry-run"], env=self.env,
                              capture_output=True, text=True, timeout=120)
        self.assertEqual(text.returncode, 0, text.stderr)
        self.assertIn("(dry run: nothing changed)", text.stdout)
        self.assertIn("largest index: %s: 4 lines" % PROJECT, text.stdout)
        self.assertEqual(snapshot(self.home), before)
        _, done = self.sync()
        self.assertEqual({name: {k: one[k] for k in keys} for name, one in done.items()}, foretold)
        self.assertEqual(foretold[PROJECT]["index"]["bytes"], len((self.shared() / INDEX).read_bytes()))

    def test_provider_profiles_keep_their_memory(self):
        own = self.note(self.lane, "lane.md", "lane note\n")
        self.note(self.default, "a.md", "note a\n")
        report, _ = self.sync()
        self.assertNotIn("lane", report["accounts"])
        self.assertFalse(self.memory(self.lane).is_symlink())
        self.assertEqual(own.read_text(), "lane note\n")
        self.assertFalse((self.shared() / "lane.md").exists())

    def test_folder_moved_aside_by_a_pass_that_stopped_is_merged_by_the_next(self):
        self.note(self.default, "a.md", "note a\n")
        self.sync()
        leftover = self.seat1 / "projects" / PROJECT / "memory.unmerged-20260101T000000Z"
        leftover.mkdir()
        (leftover / "late.md").write_text("written late\n")
        _, projects = self.sync()
        self.assertEqual(projects[PROJECT]["copied"], 1)
        self.assertEqual((self.shared() / "late.md").read_text(), "written late\n")
        self.assertFalse(leftover.exists())
        self.assertEqual((leftover.parent / "memory.backup-20260101T000000Z/late.md").read_text(), "written late\n")

    def test_project_and_account_narrow_the_pass(self):
        work = self.root / "work"
        work.mkdir()
        name = filed_under(work)
        self.note(self.default, "d.md", "d\n", project=name)
        self.note(self.seat2, "s.md", "s\n", project=name)
        self.note(self.default, "other.md", "another project\n")
        _, projects = self.sync("--project", str(work), "--account", "seat1", "--account", "seat2")
        self.assertEqual(list(projects), [name])
        self.assertEqual(projects[name]["linked"], ["seat1", "seat2"])
        self.assertFalse(self.memory(self.default, name).is_symlink())
        self.assertFalse(self.memory(self.default).is_symlink())
        self.assertEqual(sorted(p.name for p in self.shared(name).iterdir()), ["s.md"])
        before = snapshot(self.home)
        for bad in (["--account", "lane"], ["--project", "relative/path"], ["--project", str(work / "missing")]):
            with self.subTest(bad=bad):
                result = subprocess.run([sys.executable, "-B", str(HELPER), "sync", *bad], env=self.env,
                                        capture_output=True, text=True, timeout=60)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertEqual(snapshot(self.home), before)

    def test_worktree_and_subfolder_share_the_main_checkout_memory(self):
        repo = self.root / "repo"
        repo.mkdir()

        def git(*args, cwd=repo):
            subprocess.run(["git", "-c", "init.defaultBranch=main", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
                            *args], cwd=cwd, env=self.env, check=True, capture_output=True, timeout=60)

        git("init", "-q")
        git("commit", "-q", "--allow-empty", "-m", "start")
        git("worktree", "add", "-q", str(self.root / "wt"), "-b", "wt")
        (self.root / "wt/sub").mkdir()
        _, projects = self.sync("--project", str(self.root / "wt/sub"))
        self.assertEqual(list(projects), [filed_under(repo)])


class CommandTests(Homes):
    """`sb claude memory` and the launch that links a new project."""

    def setUp(self):
        super().setUp()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "launched.json"
        claude = self.bin / "claude"
        claude.write_text(f"#!{sys.executable}\n" + """
import json, os, sys
from pathlib import Path
Path(os.environ['MEMORY_LAUNCH_LOG']).write_text(json.dumps({'args': sys.argv[1:]}))
""")
        claude.chmod(0o700)
        (self.bin / "python3").symlink_to(sys.executable)
        git = shutil.which("git")
        if git:
            (self.bin / "git").symlink_to(git)
        self.env.update(PATH=f"{self.bin}:/usr/bin:/bin", SB_NATIVE_CLAUDE=str(claude),
                        SB_DEFAULT_CLAUDE_MODE="native", SB_DEFAULT_CLAUDE_ACCOUNT="default",
                        MEMORY_LAUNCH_LOG=str(self.log))

    def sb(self, *args, env=None, cwd=None):
        result = subprocess.run(["/bin/zsh", str(SB), "claude", *args], env=env or self.env, cwd=cwd,
                                capture_output=True, text=True, timeout=120)
        return result.returncode, result.stdout + result.stderr

    def test_on_links_status_counts_and_off_leaves_every_note_and_link_in_place(self):
        self.note(self.default, "a.md", "note a\n")
        self.note(self.seat1, "b.md", "note b\n")
        saved = self.runtime / "config/sb.env"
        rc, said = self.sb("memory")
        self.assertEqual(rc, 0, said)
        self.assertIn("between accounts: off", said)
        self.assertIn("default: 0 linked, 1 own folder", said)
        self.assertFalse(saved.exists())
        rc, said = self.sb("memory", "on")
        self.assertEqual(rc, 0, said)
        self.assertIn("between accounts: on", said)
        self.assertIn("export SB_CLAUDE_SHARED_MEMORY=1\n", saved.read_text())
        rc, said = self.sb("memory", "status")
        self.assertIn("seat2: 1 linked, 0 own folders", said)
        self.assertIn("(1 project)", said)
        before = snapshot(self.home, skip={".switchback/config/sb.env"})
        rc, said = self.sb("memory", "off")
        self.assertEqual(rc, 0, said)
        self.assertIn("between accounts: off", said)
        self.assertIn("export SB_CLAUDE_SHARED_MEMORY=0\n", saved.read_text())
        self.assertEqual(snapshot(self.home, skip={".switchback/config/sb.env"}), before)
        # The steps it prints give each account its own folder back, as it was before sharing.
        steps = [line.strip() for line in said.splitlines() if line.strip().startswith("rm ")]
        backup, = (self.default / "projects" / PROJECT).glob("memory.backup-*")
        memory = self.memory(self.default)
        self.assertIn("rm %s && mv %s %s" % (memory, backup, memory), steps)
        self.assertIn("rm %s" % self.memory(self.seat2), steps)
        self.assertEqual(len(steps), 3)
        for step in steps:
            subprocess.run(["/bin/sh", "-c", step], check=True, timeout=30)
        self.assertFalse(memory.is_symlink())
        self.assertEqual(sorted(p.name for p in memory.iterdir()), ["a.md"])
        self.assertFalse(os.path.lexists(self.memory(self.seat2)))
        self.assertEqual((self.shared() / "b.md").read_text(), "note b\n")
        for bad in (["sometimes"], ["off", "--dry-run"], ["status", "--project", "/x"], ["sync", "--account"]):
            with self.subTest(bad=bad):
                self.assertEqual(self.sb("memory", *bad)[0], 2)

    def test_launch_links_the_project_it_starts_in_only_when_on(self):
        work = self.root / "work repo"
        work.mkdir()
        name = filed_under(work)
        rc, said = self.sb("--account", "seat2", "-p", "hi", cwd=work)
        self.assertEqual(rc, 0, said)
        self.assertTrue(self.log.exists(), said)
        self.assertFalse(os.path.lexists(self.memory(self.seat2, name)))
        self.log.unlink()
        rc, said = self.sb("--account", "seat2", "-p", "hi", cwd=work, env=dict(self.env, SB_CLAUDE_SHARED_MEMORY="1"))
        self.assertEqual(rc, 0, said)
        self.assertEqual(json.loads(self.log.read_text())["args"][-2:], ["-p", "hi"])
        for home in (self.default, self.seat1, self.seat2):
            self.assertEqual(os.path.realpath(self.memory(home, name)), str(self.shared(name)), said)
        self.assertFalse(os.path.lexists(self.memory(self.lane, name)))


if __name__ == "__main__":
    unittest.main()
