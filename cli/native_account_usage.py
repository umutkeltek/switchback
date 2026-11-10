"""Account usage is a projection of native/vendor observations, never credentials."""
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import shutil
import stat
import time

from native_account_process import Refusal, supervised_group

SCHEMA = "switchback/account-usage@1"
FRESH_SECONDS = 900
ALIASES = {"zai-coding": "zai", "zai-coding-headroom": "zai",
           "minimax-coding": "minimax", "kimi-coding": "kimi",
           "neuralwatt-coding": "neuralwatt", "codex-relay": "codex",
           "qwen-coding": "qwencloud"}


def label(value):
    return ''.join(c for c in value if c.isprintable())[:240] if isinstance(value, str) else None


def number(value):
    try:
        return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)
    except OverflowError:
        return False


def object_value(value):
    return value if isinstance(value, dict) else {}


def account_rows(value):
    rows = object_value(value).get("accounts")
    if not isinstance(rows, list):
        return []
    return [x for x in rows if isinstance(x, dict)]


def login_revision(home):
    """Bind cached default usage to a home and login file without reading secrets."""
    try:
        info = (home / "auth.json").lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise Refusal("unsafe native login file")
    return {"home": str(home), "device": info.st_dev, "inode": info.st_ino,
            "size": info.st_size, "mtime_ns": info.st_mtime_ns, "ctime_ns": info.st_ctime_ns}


def timestamp(value):
    try:
        if number(value):
            return float(value)
        parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
        return parsed.timestamp() if parsed.tzinfo else None
    except (ValueError, TypeError, AttributeError, OverflowError):
        return None


def iso(value):
    if value is None:
        return None
    try:
        return datetime.fromtimestamp(value, timezone.utc).isoformat().replace('+00:00', 'Z')
    except (ValueError, OverflowError, OSError):
        return None


def window(name, used, reset, sampled, now, minutes=None):
    used = used if number(used) and 0 <= used <= 100 else None
    sampled = timestamp(sampled)
    reset = timestamp(reset)
    state = "unknown"
    if used is not None and sampled is not None and sampled <= now + 30:
        state = "fresh" if now - sampled <= FRESH_SECONDS else "stale"
        if reset is not None and reset <= now:
            state = "reset_passed"
    return {"name": label(name), "used_percent": used,
            "remaining_percent": 100 - used if used is not None else None,
            "resets_at": iso(reset), "reset_in_seconds": max(0, int(reset - now)) if reset is not None else None,
            "sampled_at": iso(sampled), "state": state,
            "window_minutes": minutes if number(minutes) and minutes > 0 else None}


def claude_windows(data, expected_account, now):
    cached = data.get("cachedUsageUtilization") if isinstance(data, dict) else None
    if not isinstance(cached, dict) or not expected_account or cached.get("accountUuid") != expected_account:
        return []
    fetched = cached.get("fetchedAtMs")
    usage = cached.get("utilization")
    if not number(fetched) or not isinstance(usage, dict):
        return []
    sampled = fetched / 1000
    result = []
    for name, minutes in (("five_hour", 300), ("seven_day", 10080)):
        raw = usage.get(name)
        raw = raw if isinstance(raw, dict) else {}
        result.append(window(name, raw.get("utilization"), raw.get("resets_at"), sampled, now, minutes))
    for name, raw in usage.items():
        if name.startswith("seven_day_") and isinstance(raw, dict) and "utilization" in raw:
            result.append(window(name, raw.get("utilization"), raw.get("resets_at"), sampled, now, 10080))
    return result


def summary(windows):
    result = []
    for w in windows:
        if w.get("used_percent") is None:
            continue
        name = {"five_hour": "5h", "seven_day": "week"}.get(w["name"], w["name"])
        suffix = "" if w["state"] == "fresh" else " " + w["state"]
        result.append(f'{name} {w["used_percent"]:g}% used{suffix}')
    return ', '.join(result) or "usage unknown"


def query(command, timeout):
    try:
        rc, _, _, cancelled, expired, output = supervised_group(command, os.environ.copy(), timeout, output_limit=65536)
        if cancelled:
            raise Refusal("usage refresh cancelled")
        if expired:
            return None, "timeout"
        if rc:
            return None, "source_error"
        try:
            data = json.loads(output)
        except (ValueError, UnicodeError):
            return None, "source_unavailable"
        return data, None
    except OSError:
        return None, "source_unavailable"


def codexbar_rows(payload, now):
    if not isinstance(payload, list):
        return []
    result = []
    for raw in payload:
        if not isinstance(raw, dict) or not isinstance(raw.get("usage"), dict):
            continue
        usage = raw["usage"]
        provider = label(raw.get("provider"))
        if not provider:
            continue
        identity = usage.get("identity") if isinstance(usage.get("identity"), dict) else {}
        account = label(raw.get("account") or identity.get("accountEmail")) or "configured"
        windows, notes = [], []
        for name in ("primary", "secondary", "tertiary"):
            value = usage.get(name)
            if not isinstance(value, dict):
                continue
            # Some balance-only providers encode a synthetic 0% primary window.
            # Without a duration/reset it is a description, not measured capacity.
            if number(value.get("windowMinutes")) or timestamp(value.get("resetsAt")) is not None:
                windows.append(window(name, value.get("usedPercent"), value.get("resetsAt"),
                                      usage.get("updatedAt"), now, value.get("windowMinutes")))
            elif label(value.get("resetDescription")):
                notes.append(label(value["resetDescription"]))
        balances = []
        for key in ("providerCost", "openRouterUsage"):
            value = usage.get(key)
            if not isinstance(value, dict):
                continue
            balances.append({"kind": key, **{k: value[k] for k in
                ("used", "limit", "balance", "totalCredits", "totalUsage") if number(value.get(k))},
                **{k: label(value[k]) for k in ("currencyCode", "period") if isinstance(value.get(k), str)}})
        result.append({"provider": provider, "slot": account, "email": label(identity.get("accountEmail")),
                       "source": "codexbar/" + (label(raw.get("source")) or "unknown"),
                       "observed_at": label(usage.get("updatedAt")), "windows": windows,
                       "balances": balances, "notes": notes})
    return result


def age_rows(rows, now):
    for row in rows:
        row["windows"] = [window(w["name"], w.get("used_percent"), w.get("resets_at"),
                                 w.get("sampled_at"), now, w.get("window_minutes"))
                          for w in row.get("windows", []) if isinstance(w, dict) and "name" in w]
        sampled = timestamp(row.get("observed_at"))
        row["state"] = ("fresh" if sampled is not None and -30 <= now - sampled <= FRESH_SECONDS else "stale")
        if sampled is None:
            row["state"] = "unknown"
    return rows


def history_rows(path, now):
    """Read CodexBar's own observations without writing its history or credentials."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return []
    latest = {}
    with os.fdopen(fd, 'r', encoding='utf-8') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size > 64 * 1024 * 1024:
            raise Refusal("unsafe usage history")
        for line in stream:
            if len(line) > 65536:
                raise Refusal("usage history line exceeds limit")
            try:
                raw = json.loads(line)
            except ValueError:
                raise Refusal("usage history is malformed")
            if not isinstance(raw, dict) or raw.get("provider") != "codex":
                continue
            account, kind = label(raw.get("accountKey")), label(raw.get("windowKind"))
            sampled = timestamp(raw.get("sampledAt"))
            if not account or not kind or sampled is None or sampled > now + 30:
                continue
            key = (account, kind)
            if key not in latest or timestamp(latest[key]["sampledAt"]) < sampled:
                latest[key] = raw
    grouped = {}
    for (account, kind), raw in latest.items():
        row = grouped.setdefault(account, {"provider": "codex", "slot": account,
            "source": "codexbar/history", "observed_at": None, "windows": []})
        sampled = timestamp(raw["sampledAt"])
        row["observed_at"] = iso(max(sampled, timestamp(row["observed_at"]) or 0))
        row["windows"].append(window(kind, raw.get("usedPercent"), raw.get("resetsAt"), sampled,
                                     now, raw.get("windowMinutes")))
    return list(grouped.values())


def _report(args, runtime, sb, fs, *, persist=False, native_refresh=None):
    from native_account_catalog import account_home, default_home, claude_binding
    now = time.time()
    provider = ALIASES.get(args.provider, args.provider or "all")
    if provider != "all" and not fs.valid_name(provider):
        raise Refusal("invalid usage provider")
    if args.slot and not fs.valid_name(args.slot):
        raise Refusal("invalid usage slot")
    if not 1 <= args.timeout <= 300:
        raise Refusal("usage timeout must be between 1 and 300 seconds")
    timeout = min(args.timeout, 75)
    cache_path = Path(runtime) / "state/account-usage.json"
    cached = object_value(fs.read_json(cache_path))
    if cached and cached.get("schema") != SCHEMA:
        raise Refusal("usage cache schema mismatch")
    rows, coverage, native_homes = [], [], set()
    for native in ("claude", "codex"):
        if provider not in ("all", native):
            continue
        root = Path(runtime) / "config/native-account-slots" / native
        if not root.exists():
            continue
        fs.safe_dir(root)
        for slot in sorted(root.iterdir()):
            if not fs.valid_name(slot.name) or (args.slot and args.slot != slot.name):
                continue
            fs.safe_dir(slot)
            meta = object_value(fs.read_json(slot / "slot.json"))
            if meta.get("provider") != native or meta.get("slot") != slot.name:
                continue
            refreshed, error = None, None
            # Native status serializes provider-wide identity checks; do not race slots.
            if native_refresh is not None:
                refreshed = native_refresh
            elif should_refresh(args, native, slot.name):
                refreshed, error = query([sb, "account", "status", "--provider", native,
                                           "--slot", slot.name, "--quota-refresh", "--json"], timeout)
                meta = object_value(fs.read_json(slot / "slot.json"))
            if isinstance(refreshed, dict) and refreshed.get("quota_error"):
                error = "native_usage_unavailable"
            row = {"provider": native, "slot": slot.name, "enabled": meta.get("enabled", False),
                   "account_id": meta.get("identity"), "source": "native/" + native,
                   "windows": [], "observed_at": None,
                   "relogin_required": object_value(meta.get("cooldown")).get("reason") in
                       ("authentication_failed", "unauthorized")}
            home = account_home(runtime, native, slot.name, meta)
            native_homes.add((native, str(home)))
            if native == "claude":
                metadata = Path.home() / ".claude.json" if default_home(native, home) else home / ".claude.json"
                data = object_value(fs.read_json(metadata))
                identity = object_value(data.get("oauthAccount"))
                binding = object_value(meta.get("identity"))
                row["email"] = label(identity.get("emailAddress"))
                row["observed_identity"] = claude_binding(identity.get("accountUuid"), identity.get("organizationUuid"))
                row["identity_state"] = ("unknown" if row["observed_identity"] is None else
                    "unbound" if not binding else
                    "matched" if row["observed_identity"] == binding else "changed")
                if (identity.get("accountUuid") == binding.get("account_uuid") and
                        identity.get("organizationUuid") == binding.get("organization_uuid")):
                    row["windows"] = claude_windows(data, binding.get("account_uuid"), now)
                else:
                    error = "identity_mismatch"
                row["observed_at"] = next((w["sampled_at"] for w in row["windows"] if w["sampled_at"]), None)
            else:
                revision = login_revision(home)
                if isinstance(refreshed, dict) and isinstance(refreshed.get("usage_windows"), list):
                    row["windows"] = refreshed["usage_windows"]
                    row["observed_at"] = refreshed.get("usage_observed_at")
                    row["login_revision"] = revision
                else:
                    previous = next((x for x in account_rows(cached) if x.get("provider") == native
                        and x.get("slot") == slot.name and x.get("account_id") == row["account_id"]
                        and revision and x.get("login_revision") == revision), {})
                    row["windows"] = previous.get("windows", [])
                    row["observed_at"] = previous.get("observed_at")
                    row["login_revision"] = previous.get("login_revision")
            if native_refresh is None and not should_refresh(args, native, slot.name):
                previous = next((x for x in account_rows(cached) if x.get("provider") == native
                    and x.get("slot") == slot.name and x.get("account_id") == row["account_id"]
                    and x.get("observed_at") == row["observed_at"]
                    and (native != "codex" or x.get("login_revision") == row.get("login_revision"))), {})
                error = error or previous.get("refresh_error")
            if row["relogin_required"]:
                error = "authentication_required"
            if error:
                row["refresh_error"] = error
            rows.append(row)
    codex_home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
    if (provider in ("all", "codex") and args.slot in (None, "default")
            and ("codex", str(codex_home)) not in native_homes and (codex_home / "auth.json").is_file()):
        fs.safe_dir(codex_home)
        revision = login_revision(codex_home)
        default_row = next((dict(x) for x in account_rows(cached) if
            x.get("provider") == "codex" and x.get("slot") == "default" and x.get("source") == "native/codex"
            and revision and x.get("login_revision") == revision),
            {"provider": "codex", "slot": "default", "source": "native/codex", "windows": [], "observed_at": None})
        if should_refresh(args, "codex", "default"):
            from native_codex_account import inspect_account
            try:
                observed = inspect_account(codex_home, fs.clean_env("codex", codex_home),
                                           os.environ.get("SB_NATIVE_CODEX", "codex"), min(timeout, 45))
                if observed.get("authenticated") and observed.get("_identity") and observed.get("usage_windows"):
                    default_row.update(account_id=observed["_identity"], email=label(observed.get("email")),
                                       windows=observed["usage_windows"], observed_at=observed.get("usage_observed_at"),
                                       login_revision=login_revision(codex_home))
                    default_row.pop("refresh_error", None)
                else:
                    default_row["refresh_error"] = "native_usage_unavailable"
            except (Refusal, OSError, TimeoutError):
                default_row["refresh_error"] = "native_usage_unavailable"
        rows.append(default_row)
    inventory, inventory_error = query([sb, "config", "providers"], 15)
    configured = inventory.get("providers", []) if isinstance(inventory, dict) else []
    families = {ALIASES.get(x.get("id"), x.get("id")) for x in configured if isinstance(x, dict)}
    families.discard(None)
    # Include enabled CodexBar providers without importing any of their credential fields.
    config = object_value(fs.read_json(Path.home() / ".codexbar/config.json"))
    collectors = config.get("providers")
    collectors = collectors if isinstance(collectors, list) else []
    token_counts = {x["id"]: len(account_rows(x.get("tokenAccounts")))
                    for x in collectors if isinstance(x, dict) and fs.valid_name(x.get("id", ""))}
    families.update(x["id"] for x in collectors if isinstance(x, dict)
                    and x.get("enabled") is True and fs.valid_name(x.get("id", "")))
    families.discard("claude")
    if provider != "all":
        families &= {ALIASES.get(provider, provider)}
    vendor_rows = [x for x in account_rows(cached) if str(x.get("source", "")).startswith("codexbar/")]
    if "codex" in families:
        try:
            history = history_rows(Path.home() / "Library/Application Support/CodexBar/usage-history.jsonl", now)
            if history:
                vendor_rows = [x for x in vendor_rows if x.get("provider") != "codex"] + history
        except (OSError, ValueError, Refusal):
            coverage.append({"provider": "codex", "state": "unavailable", "reason": "history_unreadable"})
    if args.refresh and shutil.which("codexbar"):
        for family in sorted(families):
            if not should_refresh(args, family):
                continue
            if family == "codex" and any(x.get("provider") == "codex" and x.get("windows") and
                    not x.get("refresh_error") for x in rows):
                continue
            mode = "cli" if family == "codex" else "api"
            selection = ["--all-accounts"] if token_counts.get(family, 0) or family == "codex" else []
            payload, error = query(["codexbar", "usage", "--provider", family, *selection, "--source", mode,
                                   "--json", "--no-credits"], timeout)
            fresh = codexbar_rows(payload, now)
            fresh = [x for x in fresh if x["provider"] == family]
            if fresh:
                vendor_rows = [x for x in vendor_rows if x.get("provider") != family] + fresh
            else:
                coverage.append({"provider": family, "state": "unavailable", "reason": error or "no_usage_source"})
                for row in vendor_rows:
                    if row.get("provider") == family:
                        row["refresh_error"] = error or "no_usage_source"
    for family in sorted(families):
        matching = [x for x in vendor_rows if x.get("provider") == family]
        if family == "codex":
            native_ids = {x.get("account_id", {}).get("value") for x in rows if
                x.get("provider") == "codex" and isinstance(x.get("account_id"), dict)}
            matching = [x for x in matching if not any(account_id and x["slot"].endswith(account_id)
                                                       for account_id in native_ids)]
        rows += matching
        if not any(x["provider"] == family for x in rows) and not any(x["provider"] == family for x in coverage):
            coverage.append({"provider": family, "state": "unknown", "reason": "no_saved_usage"})
    if inventory_error:
        coverage.append({"provider": "inventory", "state": "unavailable", "reason": inventory_error})
    if args.slot:
        rows = [x for x in rows if x["slot"] == args.slot]
    result = {"schema": SCHEMA, "generated_at": iso(time.time()), "accounts": age_rows(rows, time.time()),
              "coverage": coverage, "purpose": "usage_observation_not_admission_authority"}
    if args.refresh or persist:
        fs.safe_dir(cache_path.parent, create=True)
        with fs.lock(cache_path.parent / ".account-usage.lock"):
            prior = fs.read_json(cache_path) or {}
            preserved = [x for x in account_rows(prior) if
                (provider != "all" and x.get("provider") != provider) or
                (args.slot and x.get("slot") != args.slot)]
            saved = {**result, "accounts": preserved + result["accounts"]}
            fs.write_json(cache_path, saved)
    return result


def publish_claude_login(runtime, sb, fs, slot, result):
    """Publish the native usage evidence already refreshed by login, without probing again."""
    from types import SimpleNamespace
    args = SimpleNamespace(provider="claude", slot=slot, timeout=30, refresh=False)
    return _report(args, runtime, sb, fs, persist=True, native_refresh=result)


def render(result):
    refresh = object_value(result.get("refresh"))
    if refresh.get("state") == "in_progress":
        print("A refresh is already running; showing saved observations.")
    elif refresh.get("state") == "not_due":
        print("No collectors are due for refresh; showing saved observations.")
    print("Provider / account                  Window          Used   Left   Reset (local)             Evidence")
    for row in result["accounts"]:
        who = f'{row["provider"]}/{row["slot"]}'
        if row.get("email") and row["email"] != row["slot"]:
            who += ' ' + row["email"]
        print(f'{who} [{row["state"]}; {row["source"]}; sampled {row.get("observed_at") or "unknown"}]')
        for w in row.get("windows", []):
            used = f'{w["used_percent"]:g}%' if w.get("used_percent") is not None else "?"
            left = f'{w["remaining_percent"]:g}%' if w.get("remaining_percent") is not None else "?"
            reset = timestamp(w.get("resets_at"))
            reset_text = datetime.fromtimestamp(reset).astimezone().strftime('%b %d %H:%M %Z') if reset else "unknown"
            if w.get("reset_in_seconds") is not None:
                minutes = w["reset_in_seconds"] // 60
                reset_text += f' ({minutes // 60}h {minutes % 60}m)'
            name = w["name"]
            if w.get("window_minutes"):
                name += f' ({w["window_minutes"] / 60:g}h)'
            print(f'  {name:<33} {used:>6} {left:>6}   {reset_text:<35} {w["state"]}')
        for note in row.get("notes", []):
            print('  ' + note)
        for balance in row.get("balances", []):
            print('  ' + json.dumps(balance, sort_keys=True))
        if not row.get("windows") and not row.get("balances") and not row.get("notes"):
            print("  usage unknown")
        if row.get("refresh_error"):
            print("  Refresh unavailable; showing last saved observation: " + row["refresh_error"])
    for coverage in result["coverage"]:
        print(f'{coverage["provider"]}: {coverage["state"]} ({coverage["reason"]})')
    print("Saved observations; refresh with sb limits --refresh. Unknown is not zero usage.")

def collector_key(row):
    provider = row.get("provider", "")
    if str(row.get("source", "")).startswith("native/"):
        return provider + "/" + row.get("slot", "")
    return provider + "/*"


def refresh_groups(snapshot):
    grouped = {}
    for row in snapshot["accounts"]:
        grouped.setdefault(collector_key(row), []).append(row)
    for row in snapshot["coverage"]:
        if row["provider"] != "inventory":
            grouped.setdefault(row["provider"] + "/*", [])
    return grouped


def refresh_binding(rows):
    import hashlib
    # Changing a native account or login file invalidates timing as well as usage.
    identities = [{k: row.get(k) for k in
                   ("provider", "slot", "source", "account_id", "login_revision", "enabled", "relogin_required")}
                  for row in rows]
    identities.sort(key=lambda row: json.dumps(row, sort_keys=True))
    return hashlib.sha256(json.dumps(identities, sort_keys=True).encode()).hexdigest()


def refresh_plan(groups, schedules, sb):
    observations = []
    for key, rows in sorted(groups.items()):
        schedule = object_value(schedules.get(key))
        if schedule.get("binding") != refresh_binding(rows):
            schedule = {}
        windows = [w for row in rows for w in row.get("windows", [])]
        samples = [timestamp(row.get("observed_at")) for row in rows]
        samples = [s for s in samples if s is not None]
        used = [w["used_percent"] for w in windows if number(w.get("used_percent"))]
        resets = [timestamp(w.get("resets_at")) for w in windows]
        resets = [r for r in resets if r is not None]
        observations.append({"key": key,
            "sampled_at_ms": int(min(samples) * 1000) if samples and len(samples) == len(rows) else None,
            "used_percent": max(used) if used else None,
            "resets_at_ms": int(min(resets) * 1000) if resets else None,
            "last_attempt_at_ms": schedule.get("last_attempt_at_ms"),
            "consecutive_failures": schedule.get("consecutive_failures", 0),
            "needs_login": bool(rows) and all(row.get("relogin_required") for row in rows),
            "enabled": not rows or any(row.get("enabled", True) for row in rows)})
    result, error = query([sb, "profile", "accounts", "refresh-plan",
                           "--observations-json", json.dumps(observations), "--json"], 15)
    if error or not isinstance(result, dict) or result.get("schema") != "switchback/usage-refresh-plan@1":
        raise Refusal("Rust usage refresh policy unavailable")
    collectors = result.get("collectors")
    if (not isinstance(collectors, list) or len(collectors) != len(groups)
            or {r.get("key") for r in collectors if isinstance(r, dict)} != set(groups)
            or any(not isinstance(r.get("due"), bool) for r in collectors)):
        raise Refusal("invalid Rust usage refresh plan")
    return result


def report(args, runtime, sb, fs):
    from copy import copy
    scheduled = getattr(args, "refresh_if_due", False)
    if not args.refresh and not scheduled:
        return _report(args, runtime, sb, fs)
    if args.refresh and scheduled:
        raise Refusal("choose --refresh or --refresh-if-due")
    state = Path(runtime) / "state"
    fs.safe_dir(state, create=True)
    # One batch across all providers; readers remain available. A second caller
    # returns the saved snapshot explicitly marked in_progress, never probes again.
    with fs.lock(state / ".usage-refresh.lock", coalesce=True) as acquired:
        read_args = copy(args)
        read_args.refresh = False
        snapshot = _report(read_args, runtime, sb, fs)
        if not acquired:
            snapshot["refresh"] = {"state": "in_progress", "collectors": []}
            return snapshot
        path = state / "account-usage-refresh.json"
        saved = object_value(fs.read_json(path))
        if saved and saved.get("schema") != "switchback/usage-refresh-state@1":
            raise Refusal("usage refresh state schema mismatch")
        schedules = saved.get("collectors", {})
        if not isinstance(schedules, dict) or any(not isinstance(v, dict) or
                not isinstance(v.get("binding"), str) or len(v["binding"]) != 64 or
                not isinstance(v.get("last_attempt_at_ms"), int) or
                isinstance(v["last_attempt_at_ms"], bool) or v["last_attempt_at_ms"] < 0 or
                not isinstance(v.get("consecutive_failures"), int) or
                isinstance(v["consecutive_failures"], bool) or
                not 0 <= v["consecutive_failures"] <= 32 for v in schedules.values()):
            raise Refusal("invalid usage refresh checkpoint")
        groups = refresh_groups(snapshot)
        plan = refresh_plan(groups, schedules, sb) if scheduled else None
        due = {r["key"] for r in plan["collectors"] if r["due"]} if plan else set(groups)
        if not due:
            snapshot["refresh"] = {"state": "not_due", "collectors": plan["collectors"] if plan else []}
            return snapshot
        started = int(time.time() * 1000)
        for key in due:
            prior = object_value(schedules.get(key))
            binding = refresh_binding(groups[key])
            failures = prior.get("consecutive_failures", 0) if prior.get("binding") == binding else 0
            failures = failures if isinstance(failures, int) and 0 <= failures < 32 else 0
            # Persist before network IO. A crash or cancelled batch backs off too.
            schedules[key] = {"binding": binding, "last_attempt_at_ms": started,
                              "consecutive_failures": min(32, failures + 1)}
        fs.write_json(path, {"schema": "switchback/usage-refresh-state@1", "collectors": schedules})
        run_args = copy(args)
        run_args.refresh = True
        run_args._refresh_keys = due
        result = _report(run_args, runtime, sb, fs)
        finished = int(time.time() * 1000)
        for key in due:
            schedules[key]["last_attempt_at_ms"] = finished
        for key, rows in refresh_groups(result).items():
            if key not in due:
                continue
            schedules[key]["binding"] = refresh_binding(rows)
            # A successful source must provide an actual observation; a saved
            # reading or a transport error does not clear failure backoff.
            if rows and all(not row.get("refresh_error") and
                    (timestamp(row.get("observed_at")) or 0) * 1000 >= started - 30_000
                    for row in rows):
                schedules[key]["consecutive_failures"] = 0
        fs.write_json(path, {"schema": "switchback/usage-refresh-state@1", "collectors": schedules})
        result["refresh"] = {"state": "completed", "requested_collectors": sorted(due),
                             "collectors": plan["collectors"] if plan else []}
        return result


def should_refresh(args, provider, slot="*"):
    selected = getattr(args, "_refresh_keys", None)
    return args.refresh and (selected is None or provider + "/" + slot in selected)
