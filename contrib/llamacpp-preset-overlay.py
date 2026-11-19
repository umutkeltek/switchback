#!/usr/bin/env python3
"""Merge Switchback's managed model settings into a llama.cpp presets INI.

Switchback's engine control (a lane with `engine: {kind: llama_cpp_router,
settings_command: ...}`) runs a command with the managed overlay as JSON on
stdin, then asks the router to re-read its presets (`GET /models?reload=1`).
This script is the reference command:

    settings_command: "python3 /opt/switchback/llamacpp-preset-overlay.py \
        --base /etc/llama/models.base.ini --out /etc/llama/models.ini"

It reads the hand-written BASE file and writes OUT (the file the router reads
with `--models-preset`):

- every hand-written section, key and comment of BASE is kept in order;
- a key the overlay sets replaces the hand-written value in that section and
  is marked with a `; managed by switchback` comment line; a key the section
  lacks is added at its end under the same mark;
- an overlay model with a `base` is a variant: a new section that copies the
  base model's keys and adds its own;
- a key BASE marks as pinned is never changed: a line
  `; switchback-pinned: ctx-size, n-gpu-layers` in a section pins those keys
  there (in `[*]`, everywhere). An overlay that would change a pinned key is
  refused (exit 2) and OUT is left as it was;
- a model that is neither a section of BASE nor a variant of one is refused
  (exit 2): its section would have no `model =` line;
- OUT is written atomically (a temporary file in the same folder, then a
  rename), and only when its content changes.

The overlay looks like:

    {"version": 1, "lane": "text", "engine": "llama_cpp_router",
     "models": {"qwen3-35b": {"settings": {"ctx-size": "65536"}},
                "qwen3-35b~1a2b3c4d": {"base": "qwen3-35b",
                                       "settings": {"ctx-size": "131072"}}}}

Standard library only. Exit codes: 0 written (or unchanged), 1 cannot read or
write a file, 2 the overlay is refused.
"""

import argparse
import json
import os
import re
import sys
import tempfile

MARK = "; managed by switchback"
PIN = re.compile(r"^\s*[;#]\s*switchback-pinned\s*:\s*(.*)$")
SECTION = re.compile(r"^\s*\[(.+)\]\s*$")
KEY = re.compile(r"^\s*([A-Za-z0-9_.-]+)\s*=\s*(.*?)\s*$")
NAME_OK = re.compile(r"^[A-Za-z0-9_.-]+$")


class Refused(Exception):
    pass


def parse(text):
    """[(section or None, [lines])] in file order; None is the preamble."""
    sections, name, lines = [], None, []
    for line in text.splitlines():
        match = SECTION.match(line)
        if match:
            sections.append((name, lines))
            name, lines = match.group(1).strip(), []
        lines.append(line)
    sections.append((name, lines))
    return sections


def keys_of(lines):
    out = {}
    for line in lines:
        if line.lstrip().startswith((";", "#")):
            continue
        match = KEY.match(line)
        if match:
            out[match.group(1)] = match.group(2)
    return out


def pinned_of(lines):
    pins = set()
    for line in lines:
        match = PIN.match(line)
        if match:
            pins.update(part.strip() for part in match.group(1).split(",") if part.strip())
    return pins


def check_value(model, key, value):
    if not NAME_OK.match(key):
        raise Refused(f"{model}: `{key}` is not a key name")
    text = str(value)
    if any(c in text for c in "\r\n\0"):
        raise Refused(f"{model}: `{key}` has a line break")
    return text


def section_text(name, lines, settings):
    """A hand-written section with the overlay's keys applied."""
    out, done = [], set()
    for line in lines:
        match = None if line.lstrip().startswith((";", "#")) else KEY.match(line)
        if match and match.group(1) in settings:
            key = match.group(1)
            if key in done:
                continue  # a repeated key: the managed value stands once
            out.append(MARK)
            out.append(f"{key} = {settings[key]}")
            done.add(key)
        else:
            out.append(line)
    rest = [key for key in settings if key not in done]
    if rest:
        while out and not out[-1].strip():
            out.pop()
        out.append(MARK)
        out.extend(f"{key} = {settings[key]}" for key in rest)
        out.append("")
    return out


def merge(base_text, overlay):
    if not isinstance(overlay, dict) or not isinstance(overlay.get("models", {}), dict):
        raise Refused("the overlay must be an object with `models`")
    sections = parse(base_text)
    by_name = {name: lines for name, lines in sections if name is not None}
    global_pins = pinned_of(by_name.get("*", []))
    models = {}
    for model, entry in overlay.get("models", {}).items():
        if not NAME_OK.match(model.replace("/", "_").replace(":", "_").replace("~", "_")):
            raise Refused(f"`{model}` is not a model name")
        if not isinstance(entry, dict) or not isinstance(entry.get("settings", {}), dict):
            raise Refused(f"{model}: `settings` must be an object")
        settings = {key: check_value(model, key, value)
                    for key, value in (entry.get("settings") or {}).items()}
        base = entry.get("base")
        own = by_name.get(model)
        if own is None and (base is None or base not in by_name):
            # A typo or a model the base file lacks would become a section
            # with no `model =` line, which the router cannot load.
            if base is None:
                raise Refused(f"{model}: no [{model}] section in the base file")
            raise Refused(f"{model}: variant of [{base}], which the base file lacks")
        pins = set(global_pins)
        if own is not None:
            pins |= pinned_of(own)
        if base is not None and base in by_name:
            pins |= pinned_of(by_name[base])
        reference = keys_of(own if own is not None else by_name.get(base, []))
        for key, value in settings.items():
            if key in pins and reference.get(key) != value:
                raise Refused(f"{model}: `{key}` is pinned in the base file")
        models[model] = (base, settings)

    out = []
    seen = set()
    for name, lines in sections:
        if name is not None and name in models:
            out.extend(section_text(name, lines, models[name][1]))
            seen.add(name)
        else:
            out.extend(lines)
    for model, (base, settings) in models.items():
        if model in seen:
            continue
        while out and not out[-1].strip():
            out.pop()
        out.append("")
        out.append(f"[{model}]")
        if base is not None:
            out.append(f"; variant of [{base}], managed by switchback")
            inherited = keys_of(by_name.get(base, []))
            for key, value in inherited.items():
                if key not in settings:
                    out.append(f"{key} = {value}")
        out.append(MARK)
        out.extend(f"{key} = {value}" for key, value in settings.items())
    return "\n".join(out).rstrip("\n") + "\n"


def write_atomic(path, text):
    folder = os.path.dirname(os.path.abspath(path)) or "."
    fd, tmp = tempfile.mkstemp(prefix=".preset-", suffix=".ini", dir=folder)
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        if os.path.exists(path):
            os.chmod(tmp, os.stat(path).st_mode & 0o777)
        os.replace(tmp, path)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base", required=True, help="the hand-written presets INI")
    parser.add_argument("--out", required=True, help="the INI the router reads")
    args = parser.parse_args(argv)
    try:
        overlay = json.load(sys.stdin)
    except ValueError as error:
        print(f"overlay is not JSON: {error}", file=sys.stderr)
        return 2
    try:
        with open(args.base, encoding="utf-8") as handle:
            base_text = handle.read()
    except OSError as error:
        print(f"cannot read the base file: {error}", file=sys.stderr)
        return 1
    try:
        text = merge(base_text, overlay)
    except Refused as error:
        print(f"refused: {error}", file=sys.stderr)
        return 2
    try:
        with open(args.out, encoding="utf-8") as handle:
            if handle.read() == text:
                print("unchanged")
                return 0
    except OSError:
        pass
    try:
        write_atomic(args.out, text)
    except OSError as error:
        print(f"cannot write {args.out}: {error}", file=sys.stderr)
        return 1
    print(f"wrote {len(overlay.get('models', {}))} managed model(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
