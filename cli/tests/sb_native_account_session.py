#!/usr/bin/env python3
"""A chat started under one saved Claude account can be resumed under another."""
import fcntl
import json
import os
import pty
import select
from pathlib import Path
import subprocess
import sys
import tempfile
import termios
import time
import unittest


CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
FOLLOW = CLI / "native_account_session.py"
CHAT = "e32ba48b-0205-48c5-b8f8-67115942dd90"
PROJECT = "-work-repo"


def line(record):
    """One chat line, written the way Claude Code writes it."""
    return json.dumps(record, separators=(",", ":")).encode() + b"\n"


def turn(minute, role="user"):
    """Something the person or the model said at `minute`."""
    return line({"type": role, "timestamp": "2026-01-01T00:%02d:00.000Z" % minute,
                 "message": {"role": role, "content": "said at minute %d" % minute}})


def opened(minute):
    """What Claude Code adds to a chat that is opened and closed with nothing asked."""
    return b"".join(line(record) for record in (
        {"type": "assistant", "timestamp": "2026-01-01T00:%02d:00.000Z" % minute,
         "message": {"role": "assistant", "model": "<synthetic>",
                     "content": [{"type": "text", "text": "No response requested."}]}},
        {"type": "attachment", "timestamp": "2026-01-01T00:%02d:01.000Z" % minute,
         "attachment": {"type": "hook_success"}},
        {"type": "system", "subtype": "bridge_status", "timestamp": "2026-01-01T00:%02d:02.000Z" % minute},
        {"type": "last-prompt", "lastPrompt": "said at minute 1", "sessionId": CHAT},
        {"type": "session-owner", "accountUuid": "owner", "sessionId": CHAT},
    ))


class SessionFollowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sb-session-follow-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.runtime = self.home / ".switchback"
        self.profiles = self.runtime / "config/claude"
        self.default = self.home / ".claude"
        self.seat1 = self.profiles / "seat1"
        self.seat2 = self.profiles / "seat2"
        for path in (self.default, self.seat1, self.seat2, self.profiles / "_providers/lane"):
            path.mkdir(parents=True)
        (self.runtime / "manifest.json").write_text("{}")
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("SB_", "SWITCHBACK_", "CLAUDE_", "ANTHROPIC_"))}
        self.env.update(HOME=str(self.home), SWITCHBACK_RUNTIME_ROOT=str(self.runtime))

    def chat(self, home, body, age=0, project=PROJECT):
        """Write a chat under `home`; a larger `age` makes it older."""
        path = home / "projects" / project / (CHAT + ".jsonl")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body)
        path.chmod(0o600)
        os.utime(path, (1_800_000_000 - age, 1_800_000_000 - age))
        return path

    def follow(self, slot="seat2", session=CHAT):
        result = subprocess.run([sys.executable, "-B", str(FOLLOW), "--slot", slot, "--session", session, "--json"],
                                env=self.env, capture_output=True, text=True, timeout=20)
        return result.returncode, (json.loads(result.stdout) if result.stdout.strip() else None), result.stderr

    def test_chat_side_files_and_checkpoints_follow_to_the_launched_account(self):
        source = self.chat(self.seat1, b'{"n":1}\n{"n":2}\n')
        side = self.seat1 / "projects" / PROJECT / CHAT / "subagents/agent-a.jsonl"
        side.parent.mkdir(parents=True)
        side.write_text("agent")
        checkpoint = self.seat1 / "file-history" / CHAT / "abc@v1"
        checkpoint.parent.mkdir(parents=True)
        checkpoint.write_text("before")
        rc, report, err = self.follow()
        self.assertEqual(rc, 0, err)
        self.assertEqual((report["copied"], report["from"], report["side_files"]), (True, "seat1", 2))
        copy = self.seat2 / "projects" / PROJECT / (CHAT + ".jsonl")
        self.assertEqual(copy.read_bytes(), source.read_bytes())
        self.assertEqual(copy.stat().st_mtime_ns, source.stat().st_mtime_ns)
        self.assertEqual(copy.stat().st_mode & 0o777, 0o600)
        self.assertEqual((self.seat2 / "projects" / PROJECT / CHAT / "subagents/agent-a.jsonl").read_text(), "agent")
        self.assertEqual((self.seat2 / "file-history" / CHAT / "abc@v1").read_text(), "before")
        self.assertEqual(source.read_bytes(), b'{"n":1}\n{"n":2}\n')
        self.assertEqual(sorted(p.name for p in copy.parent.iterdir()), [CHAT, CHAT + ".jsonl"])
        # A second launch finds nothing left to do.
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"]), (0, False), err)

    def test_default_profile_is_an_account_too(self):
        self.chat(self.default, b"from default\n")
        rc, report, err = self.follow("seat1")
        self.assertEqual((rc, report["from"]), (0, "default"), err)
        self.chat(self.seat2, b"from default\nand more\n", age=-5)
        rc, report, err = self.follow("default")
        self.assertEqual((rc, report["from"]), (0, "seat2"), err)
        self.assertEqual((self.default / "projects" / PROJECT / (CHAT + ".jsonl")).read_bytes(),
                         b"from default\nand more\n")

    def test_older_copy_is_replaced_only_by_its_own_continuation(self):
        self.chat(self.seat2, b"one\n", age=60)
        self.chat(self.seat1, b"one\ntwo\n")
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"]), (0, True), err)
        self.assertEqual((self.seat2 / "projects" / PROJECT / (CHAT + ".jsonl")).read_bytes(), b"one\ntwo\n")

    def test_copy_that_went_its_own_way_is_kept(self):
        mine = self.chat(self.seat2, b"one\nmine\n", age=60)
        self.chat(self.seat1, b"one\ntheirs and longer\n")
        rc, report, err = self.follow()
        self.assertEqual(rc, 1)
        self.assertIsNone(report)
        self.assertIn("different continuation", err)
        self.assertEqual(mine.read_bytes(), b"one\nmine\n")

    def test_newer_copy_in_the_launched_account_is_never_touched(self):
        mine = self.chat(self.seat2, b"one\ntwo\nthree\n")
        self.chat(self.seat1, b"one\n", age=60)
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"]), (0, False), err)
        self.assertEqual(mine.read_bytes(), b"one\ntwo\nthree\n")

    def test_copy_that_was_only_opened_does_not_pass_for_the_newest(self):
        # seat1 carried the chat on. seat2 then opened its old copy and asked nothing,
        # which gave that copy the newest file time and lines seat1's copy lacks.
        further = self.chat(self.seat1, turn(1) + turn(2, "assistant") + turn(3) + turn(4, "assistant"), age=600)
        stale = turn(1) + turn(2, "assistant") + opened(50)
        mine = self.chat(self.seat2, stale)
        # Run it the way the launcher does, to read what the person is told.
        told = subprocess.run([sys.executable, "-B", str(FOLLOW), "--slot", "seat2", "--session", CHAT],
                              env=self.env, capture_output=True, text=True, timeout=20)
        self.assertEqual(told.returncode, 0, told.stderr)
        self.assertEqual(mine.read_bytes(), further.read_bytes())
        kept, = [p for p in mine.parent.iterdir() if p.name.startswith(CHAT + ".jsonl.replaced-")]
        self.assertIn("copied from seat1 to seat2", told.stderr)
        self.assertIn("is kept as " + kept.name, told.stderr)
        self.assertEqual(kept.read_bytes(), stale)
        self.assertEqual(sorted(p.name for p in mine.parent.iterdir()), [CHAT + ".jsonl", kept.name])
        # The second run finds nothing left to do and sets nothing else aside.
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"]), (0, False), err)
        self.assertEqual(sorted(p.name for p in mine.parent.iterdir()), [CHAT + ".jsonl", kept.name])

    def test_something_said_in_the_launched_copy_is_never_replaced(self):
        self.chat(self.seat1, turn(1) + turn(2, "assistant") + turn(9) + turn(10, "assistant"), age=600)
        forked = turn(1) + turn(2, "assistant") + opened(3) + turn(4)
        mine = self.chat(self.seat2, forked)
        rc, report, err = self.follow()
        self.assertEqual(rc, 1)
        self.assertIsNone(report)
        self.assertIn("different continuation", err)
        self.assertEqual(mine.read_bytes(), forked)
        self.assertEqual([p.name for p in mine.parent.iterdir()], [CHAT + ".jsonl"])

    def test_line_that_cannot_be_read_counts_as_said(self):
        self.chat(self.seat1, turn(1) + turn(2, "assistant") + turn(9), age=600)
        damaged = turn(1) + turn(2, "assistant") + opened(3) + b'{"type":"user","timestamp":"2026-01-0'
        mine = self.chat(self.seat2, damaged)
        rc, _, err = self.follow()
        self.assertEqual(rc, 1)
        self.assertIn("different continuation", err)
        self.assertEqual(mine.read_bytes(), damaged)

    def test_last_thing_said_decides_between_other_accounts_not_file_times(self):
        further = self.chat(self.seat1, turn(1) + turn(2, "assistant") + turn(3), age=600)
        self.chat(self.seat2, turn(1) + turn(2, "assistant") + opened(50))
        rc, report, err = self.follow("default")
        self.assertEqual((rc, report["copied"], report["from"]), (0, True, "seat1"), err)
        self.assertEqual((self.default / "projects" / PROJECT / (CHAT + ".jsonl")).read_bytes(), further.read_bytes())

    def test_own_copy_holding_the_last_thing_said_is_kept_when_another_was_only_opened(self):
        body = turn(1) + turn(2, "assistant") + turn(3)
        mine = self.chat(self.seat2, body, age=600)
        self.chat(self.seat1, turn(1) + turn(2, "assistant") + opened(50))
        self.chat(self.default, body + opened(55), age=-5)
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"]), (0, False), err)
        self.assertEqual(mine.read_bytes(), body)
        self.assertEqual([p.name for p in mine.parent.iterdir()], [CHAT + ".jsonl"])

    def test_chat_still_open_under_another_account_is_not_copied(self):
        self.chat(self.seat1, b"live\n")
        sessions = self.seat1 / "sessions"
        sessions.mkdir()
        (sessions / "1.json").write_text(json.dumps({"pid": os.getpid(), "sessionId": CHAT}))
        rc, _, err = self.follow()
        self.assertEqual(rc, 1)
        self.assertIn("still open under seat1", err)
        self.assertFalse((self.seat2 / "projects").exists())
        # A record whose process is gone does not hold the chat.
        done = subprocess.Popen([sys.executable, "-c", "pass"])
        done.wait()
        (sessions / "1.json").write_text(json.dumps({"pid": done.pid, "sessionId": CHAT}))
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"]), (0, True), err)

    def test_links_and_transport_profiles_are_not_sources(self):
        outside = self.chat(self.root / "outside", b"outside\n")
        linked = self.seat1 / "projects" / PROJECT / (CHAT + ".jsonl")
        linked.parent.mkdir(parents=True)
        linked.symlink_to(outside)
        self.chat(self.profiles / "_providers/lane", b"lane\n")
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"], report["skipped"]), (0, False, "no account holds this chat"), err)
        self.assertFalse((self.seat2 / "projects").exists())

    def test_unknown_chat_bad_id_and_missing_account_change_nothing(self):
        before = sorted(str(p) for p in self.home.rglob("*"))
        rc, report, err = self.follow()
        self.assertEqual((rc, report["copied"]), (0, False), err)
        for slot, session in (("seat2", "../../etc/passwd"), ("seat2", CHAT.upper()), ("nobody", CHAT), ("../seat1", CHAT)):
            with self.subTest(slot=slot, session=session):
                rc, report, err = self.follow(slot, session)
                self.assertEqual(rc, 1)
                self.assertIn("chat not copied", err)
        self.assertEqual(before, sorted(str(p) for p in self.home.rglob("*")))


class ResumeLaunchTests(unittest.TestCase):
    """`sb claude --account NAME --resume ID` brings the chat along before the vendor starts."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sb-resume-launch-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.runtime = self.home / ".switchback"
        self.profiles = self.runtime / "config/claude"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        (self.home / ".claude").mkdir(parents=True)
        self.profiles.mkdir(parents=True)
        (self.runtime / "manifest.json").write_text("{}")
        for slot in ("seat1", "seat2"):
            (self.profiles / slot).mkdir()
            (self.profiles / slot / ".claude.json").write_text(
                json.dumps({"oauthAccount": {"emailAddress": slot + "@example.invalid"}}))
        (self.home / ".claude.json").write_text(json.dumps({"oauthAccount": {"emailAddress": "d@example.invalid"}}))
        self.log = self.root / "launched.json"
        claude = self.bin / "claude"
        claude.write_text(f"#!{sys.executable}\n" + """
import json, os, sys
from pathlib import Path
home = Path(os.environ.get('CLAUDE_CONFIG_DIR') or Path.home() / '.claude')
Path(os.environ['RESUME_LAUNCH_LOG']).write_text(json.dumps({
    'config': os.environ.get('CLAUDE_CONFIG_DIR'), 'args': sys.argv[1:],
    'chats': sorted(p.name for p in home.glob('projects/*/*.jsonl'))}))
""")
        claude.chmod(0o700)
        (self.bin / "python3").symlink_to(sys.executable)
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("SB_", "SWITCHBACK_", "CLAUDE_", "ANTHROPIC_"))}
        self.env.update(HOME=str(self.home), PATH=f"{self.bin}:/usr/bin:/bin",
                        SWITCHBACK_RUNTIME_ROOT=str(self.runtime), SB_NATIVE_CLAUDE=str(claude),
                        SB_DEFAULT_CLAUDE_MODE="native", SB_DEFAULT_CLAUDE_ACCOUNT="default",
                        RESUME_LAUNCH_LOG=str(self.log))
        chat = self.profiles / "seat1/projects" / PROJECT / (CHAT + ".jsonl")
        chat.parent.mkdir(parents=True)
        chat.write_text("started under seat1\n")

    def launch(self, *args):
        self.log.unlink(missing_ok=True)
        result = subprocess.run(["/bin/zsh", str(SB), "claude", *args], env=self.env,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(self.log.read_text()), result.stderr

    def test_resume_by_id_brings_the_chat_to_the_launched_account(self):
        for flags in (["--resume", CHAT], ["-r", CHAT], ["--resume=" + CHAT]):
            with self.subTest(flags=flags):
                copy = self.profiles / "seat2/projects" / PROJECT / (CHAT + ".jsonl")
                copy.unlink(missing_ok=True)
                launched, err = self.launch("--account", "seat2", *flags, "--fork-session")
                self.assertEqual(launched["config"], str(self.profiles / "seat2"))
                self.assertEqual(launched["chats"], [CHAT + ".jsonl"])
                self.assertEqual(launched["args"][-len(flags) - 1:], [*flags, "--fork-session"])
                self.assertIn("copied from seat1 to seat2", err)

    def test_resume_without_account_brings_the_chat_to_the_default_account(self):
        launched, _ = self.launch("--resume", CHAT)
        self.assertIsNone(launched["config"])
        self.assertEqual(launched["chats"], [CHAT + ".jsonl"])

    def test_other_launches_copy_nothing(self):
        for args in (["--resume"], ["--continue"], ["--print", "--", "--resume", CHAT],
                     ["--resume", "a-title-not-an-id"], []):
            with self.subTest(args=args):
                launched, err = self.launch("--account", "seat2", *args)
                self.assertEqual(launched["chats"], [])
                self.assertNotIn("copied", err)

    def test_chat_that_cannot_be_copied_never_blocks_the_launch(self):
        mine = self.profiles / "seat2/projects" / PROJECT / (CHAT + ".jsonl")
        mine.parent.mkdir(parents=True)
        mine.write_text("a different chat body\n")
        os.utime(mine, (1_000_000_000, 1_000_000_000))
        launched, err = self.launch("--account", "seat2", "--resume", CHAT)
        self.assertEqual(launched["args"][-2:], ["--resume", CHAT])
        self.assertIn("chat not copied to seat2", err)
        self.assertEqual(mine.read_text(), "a different chat body\n")


    def test_reason_a_chat_was_not_copied_stays_on_screen_until_a_key_is_pressed(self):
        # Claude Code redraws the screen as it starts, so at a terminal the launch
        # holds the reason until a key is pressed. Without a terminal it never waits
        # (test_chat_that_cannot_be_copied_never_blocks_the_launch).
        mine = self.profiles / "seat2/projects" / PROJECT / (CHAT + ".jsonl")
        mine.parent.mkdir(parents=True)
        mine.write_text("a different chat body\n")
        os.utime(mine, (1_000_000_000, 1_000_000_000))
        self.log.unlink(missing_ok=True)
        screen, terminal = pty.openpty()
        self.addCleanup(os.close, screen)
        process = subprocess.Popen(
            ["/bin/zsh", str(SB), "claude", "--account", "seat2", "--resume", CHAT], env=self.env,
            stdin=terminal, stdout=subprocess.DEVNULL, stderr=terminal, start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0))
        self.addCleanup(process.kill)
        os.close(terminal)

        def shown(until, wait):
            text, deadline = "", time.monotonic() + wait
            while until not in text and time.monotonic() < deadline:
                if select.select([screen], [], [], 0.2)[0]:
                    try:
                        text += os.read(screen, 65536).decode("utf-8", "replace")
                    except OSError:
                        break
            return text

        text = shown("Press a key", 20)
        self.assertIn("chat not copied to seat2", text)
        self.assertIn("Press a key", text)
        time.sleep(1)
        self.assertFalse(self.log.exists(), "the launch did not wait for the key")
        os.write(screen, b"x")
        self.assertEqual(process.wait(timeout=20), 0)
        self.assertEqual(json.loads(self.log.read_text())["args"][-2:], ["--resume", CHAT])
        self.assertEqual(mine.read_text(), "a different chat body\n")


if __name__ == "__main__":
    unittest.main()
