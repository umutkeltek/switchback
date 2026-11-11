#!/usr/bin/env python3
"""Make a saved Claude account start with the default Claude profile's setup.

The default profile is the one source: settings in ~/.claude, user MCP servers
in ~/.claude.json. Before an interactive `sb claude --account NAME` launch, sb
copies that setup into the account's own Claude home:

  settings.json        the whole file; local edits are saved under backups/ first
  keybindings.json     the whole file
  skills/ agents/ commands/ output-styles/
                       one link per default entry, so additions and removals follow
  .claude.json         only the user-scope "mcpServers" value

Login, usage, history, projects and every other .claude.json value stay with the
account. A default settings file that sets credentials or the provider route is
refused, because copying it would move an account onto another identity.

usage: native_account_mirror.py --slot NAME [--json]
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import stat
import sys
import tempfile
import time
from pathlib import Path

from native_account_env import Refusal, validate_settings_env

LIMIT = 16 * 1024 * 1024
LINKED_DIRS = ("skills", "agents", "commands", "output-styles")
WHOLE_FILES = ("keybindings.json",)
BACKUPS_KEPT = 10
STATE = ".switchback-default-mirror.json"
CREDENTIAL_KEYS = ("apiKeyHelper", "awsAuthRefresh", "awsCredentialExport", "vertexAuthRefresh")
BLOCKED_ENV = ("ANTHROPIC_", "OPENAI_", "CLAUDE_CODE_OAUTH", "CLAUDE_CODE_USE_")


def default_home():
    override = os.environ.get("SB_CLAUDE_DEFAULT_HOME")
    return Path(override) if override else Path.home() / ".claude"


def default_global_config():
    override = os.environ.get("SB_CLAUDE_DEFAULT_GLOBAL_CONFIG")
    return Path(override) if override else Path.home() / ".claude.json"


def read_owned(path):
    """Bytes of an owned regular file nobody else can write; None when absent."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    except OSError as e:
        raise Refusal("unsafe file: %s" % path) from e
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o022 or info.st_size > LIMIT):
            raise Refusal("unsafe file: %s" % path)
        return stream.read(LIMIT + 1)


def write_atomic(path, data, mode=0o600):
    if os.path.islink(path):
        raise Refusal("refusing to replace a link: %s" % path)
    fd, temporary = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)
        raise


def check_settings(body):
    try:
        settings = json.loads(body)
    except (ValueError, UnicodeDecodeError) as e:
        raise Refusal("default settings are not valid JSON") from e
    if not isinstance(settings, dict):
        raise Refusal("default settings must be a JSON object")
    if any(key in settings for key in CREDENTIAL_KEYS):
        raise Refusal("default settings set credentials; not copying them to an account")
    env = settings.get("env", {})
    if not isinstance(env, dict):
        raise Refusal("default settings env must be an object")
    validate_settings_env(env)
    if any(not isinstance(k, str) or k.upper().startswith(BLOCKED_ENV)
           or k.upper() in ("CLAUDE_CONFIG_DIR", "CODEX_HOME") for k in env):
        raise Refusal("default settings set a credential or provider route; not copying them to an account")
    return settings


def local_keys(current_body, wanted):
    """Top-level keys whose account value would be replaced or dropped."""
    try:
        current = json.loads(current_body)
    except (ValueError, UnicodeDecodeError):
        return ["<unreadable file>"]
    if not isinstance(current, dict):
        return ["<not an object>"]
    return sorted(k for k, v in current.items() if k not in wanted or wanted[k] != v)


def backup(home, name, body):
    folder = home / "backups"
    if folder.is_symlink():
        raise Refusal("account backups folder is a link")
    folder.mkdir(mode=0o700, exist_ok=True)
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    target = folder / ("%s.%s.before-default" % (name, stamp))
    write_atomic(target, body)
    olds = sorted(p for p in folder.iterdir() if p.name.startswith(name + ".") and p.name.endswith(".before-default"))
    for old in olds[:-BACKUPS_KEPT]:
        old.unlink()
    return target


def digest(body):
    return hashlib.sha256(body).hexdigest()


def load_state(home):
    body = read_owned(home / STATE)
    try:
        state = json.loads(body) if body else {}
    except (ValueError, UnicodeDecodeError):
        state = {}
    return state if isinstance(state, dict) else {}


def same_json(current, body):
    """Same content in another layout (Switchback's pool sync re-renders settings)."""
    try:
        return current is not None and json.loads(current) == json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return False


def edited_here(current, name, state):
    """True when the account's file is not the copy this module last wrote."""
    return current is not None and state.get(name) != digest(current)


def mirror_settings(src_home, home, state, report):
    body = read_owned(src_home / "settings.json")
    if body is None:
        report["notes"].append("default has no settings.json")
        return
    wanted = check_settings(body)
    target = home / "settings.json"
    current = read_owned(target)
    if same_json(current, body):
        state["settings.json"] = digest(current)
        return
    if edited_here(current, "settings.json", state):
        replaced = local_keys(current, wanted)
        if replaced:
            report["settings_reset"] = replaced
            report["backup"] = str(backup(home, "settings.json", current))
    write_atomic(target, body)
    state["settings.json"] = digest(body)
    report["copied"].append("settings.json")


def mirror_file(src_home, home, name, state, report):
    body = read_owned(src_home / name)
    if body is None:
        return
    target = home / name
    current = read_owned(target)
    if current == body or same_json(current, body):
        state[name] = digest(current)
        return
    if edited_here(current, name, state):
        backup(home, name, current)
    write_atomic(target, body)
    state[name] = digest(body)
    report["copied"].append(name)


def mirror_links(src_home, home, name, report):
    source = src_home / name
    if not source.is_dir():
        return
    target = home / name
    if target.is_symlink() or (target.exists() and not target.is_dir()):
        report["notes"].append("%s/ in the account is not a plain folder; left alone" % name)
        return
    target.mkdir(mode=0o755, exist_ok=True)
    added = kept = removed = 0
    for entry in sorted(os.listdir(source)):
        if entry.startswith("."):
            continue
        link, wanted = target / entry, str(source / entry)
        if os.path.lexists(link):
            if not (link.is_symlink() and os.readlink(link) == wanted):
                kept += 1
            continue
        os.symlink(wanted, link)
        added += 1
    prefix = str(source) + os.sep
    for entry in os.listdir(target):
        link = target / entry
        if link.is_symlink() and os.readlink(link).startswith(prefix) and not os.path.lexists(source / entry):
            link.unlink()
            removed += 1
    if added or removed:
        report["linked"][name] = {"added": added, "removed": removed}
    if kept:
        report["notes"].append("%s/: %d account-only entr%s with a default's name kept" % (name, kept, "y" if kept == 1 else "ies"))


@contextlib.contextmanager
def claude_config_lock(path, wait=2.0):
    """Hold Claude Code's own lock folder for its global config, or give up."""
    lock = path.with_name(path.name + ".lock")
    deadline = time.monotonic() + wait
    while True:
        try:
            os.mkdir(lock, 0o700)
            break
        except FileExistsError:
            if time.monotonic() >= deadline:
                yield False
                return
            time.sleep(0.05)
    try:
        yield True
    finally:
        with contextlib.suppress(OSError):
            os.rmdir(lock)


def mirror_mcp(home, report):
    source = read_owned(default_global_config())
    if source is None:
        return
    try:
        wanted = json.loads(source).get("mcpServers", {})
    except (ValueError, UnicodeDecodeError, AttributeError) as e:
        raise Refusal("default global config is not a JSON object") from e
    target = home / ".claude.json"

    def current_config():
        body = read_owned(target)
        if body is None:
            return None
        config = json.loads(body)
        if not isinstance(config, dict):
            raise Refusal("account global config is not a JSON object")
        return config

    config = current_config()
    if config is None or config.get("mcpServers", {}) == wanted:
        return
    with claude_config_lock(target) as held:
        if not held:
            report["notes"].append("MCP servers not copied: Claude is saving this account's config; next launch retries")
            return
        config = current_config()
        if config is None or config.get("mcpServers", {}) == wanted:
            return
        config["mcpServers"] = wanted
        write_atomic(target, (json.dumps(config, indent=2, ensure_ascii=False) + "\n").encode())
    report["copied"].append("mcpServers")


def account_home(runtime, slot):
    record = Path(runtime) / "config/native-account-slots/claude" / slot / "slot.json"
    body = read_owned(record)
    if body is None:
        return None
    data = json.loads(body)
    if (not isinstance(data, dict) or data.get("provider") != "claude" or not data.get("enabled")
            or not isinstance(data.get("identity"), dict)):
        return None
    spec = data.get("native_home") or {"kind": "managed", "label": slot}
    if spec.get("kind") != "managed" or not isinstance(spec.get("label"), str):
        return None
    label = spec["label"]
    if not label or "/" in label or label.startswith(".") or label.lower() == "default":
        raise Refusal("invalid account home label")
    return Path(runtime) / "config/claude" / label


def mirror(runtime, slot):
    report = {"slot": slot, "copied": [], "linked": {}, "notes": []}
    home = account_home(runtime, slot)
    if home is None:
        report["skipped"] = "not a saved Claude subscription account"
        return report
    if home.is_symlink() or not home.is_dir():
        raise Refusal("account home is missing or a link: %s" % home)
    src_home = default_home()
    if not src_home.is_dir():
        report["skipped"] = "no default Claude profile"
        return report
    if os.path.realpath(src_home) == os.path.realpath(home):
        report["skipped"] = "account is the default profile"
        return report
    with open(home / ".switchback-default-mirror.lock", "a") as guard:
        os.chmod(guard.name, 0o600)
        fcntl.flock(guard, fcntl.LOCK_EX)
        state = load_state(home)
        before = dict(state)
        try:
            mirror_settings(src_home, home, state, report)
            for name in WHOLE_FILES:
                mirror_file(src_home, home, name, state, report)
        finally:
            if state != before:
                write_atomic(home / STATE, (json.dumps(state, indent=2, sort_keys=True) + "\n").encode())
        for name in LINKED_DIRS:
            mirror_links(src_home, home, name, report)
        mirror_mcp(home, report)
    return report


def summary(report):
    if report.get("skipped"):
        return ""
    parts = list(report["copied"])
    parts += ["%s +%d/-%d" % (k, v["added"], v["removed"]) for k, v in sorted(report["linked"].items())]
    lines = []
    if parts:
        lines.append("default setup copied to %s: %s" % (report["slot"], ", ".join(parts)))
    if report.get("settings_reset"):
        lines.append("  account-only settings replaced by the default: %s (saved: %s)"
                     % (", ".join(report["settings_reset"]), report["backup"]))
    lines += ["  " + note for note in report["notes"]]
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--slot", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    runtime = os.environ.get("SWITCHBACK_RUNTIME_ROOT", "")
    try:
        if not runtime or not os.path.isabs(runtime):
            raise Refusal("an absolute SWITCHBACK_RUNTIME_ROOT is required")
        if not args.slot or "/" in args.slot or args.slot.startswith("."):
            raise Refusal("invalid account name")
        report = mirror(os.path.realpath(runtime), args.slot)
    except (Refusal, OSError, ValueError) as e:
        print("default setup not copied: %s" % e, file=sys.stderr)
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
