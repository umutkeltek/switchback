#!/usr/bin/env python3
"""Prepaid credit grants: what is left, when it expires, and how fast it may be spent.

Grant facts are operator-supplied in <runtime>/config/credit-grants.json (an
ordinary API key cannot read its own balance). Two readings of the spend:

1. Exact: when the grant names a usage-read key (usage_credential_ref) the
   provider's own billing since the grant date is read, one fixed-host call,
   cached ten minutes. It sees every call on the account.
2. Estimate, the fallback: the gateway's own usage store, opened read-only,
   priced here from the provider registry rows. The store's cost_micros bills
   cached input at the full input rate and is not used.

Stored usage (crates/sb-ledger record_to_event): input_tokens excludes cached
tokens, cached_input_tokens holds them, and output_tokens is the provider's own
output count, which for OpenAI includes reasoning tokens. So each token is
priced once: input, cached (cached rate, else input rate) and output.
The estimate uses list prices, and calls outside the gateway are unseen.
"""
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import stat
import sys
import tempfile
import time

SCHEMA = "switchback/credit-grants@1"
VIEW_SCHEMA = "switchback/credit-grant-view@1"
PROVIDER = re.compile(r"^[a-z0-9][a-z0-9-]{0,31}$")
ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
LIMIT = 65536
PACE_BAND = 0.15
BASIS = "gateway requests since the reading at registry list prices; calls outside the gateway are not seen"
BILLING = {"openai": ("api.openai.com", "/v1/organization/costs")}
# Readers of `sb limits` treat provider `openai` as the Codex subscription. The API credit is a
# different account, so its row carries its own provider name.
ROW_PROVIDER = {"openai": "openai-api"}
BILLING_BASIS = "the provider's own billing since the grant date; it can lag by some minutes"
BILLING_TTL, BILLING_RETRY, BILLING_KEEP = 600, 120, 90 * 86400
UNKNOWN = {"usage_store_unavailable": "the gateway usage store could not be read",
           "price_registry_unavailable": "the price registry could not be read"}
USAGE = ("usage: sb credits [--json] | sb credits observe PROVIDER --balance USD --expires DATE"
         " [--received DATE] [--amount USD] [--reserve USD]")


class GrantError(Exception):
    """A fixed nonsecret diagnostic."""


def iso(value):
    return datetime.fromtimestamp(value, timezone.utc).isoformat().replace("+00:00", "Z")


def instant(value):
    """Epoch seconds: a date is 00:00 UTC; a timestamp without an offset is UTC."""
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}([T ][0-9:.]+(Z|[+-][0-9:]+)?)?", value):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return (parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)).timestamp()
    except (ValueError, OverflowError):
        return None


def day(epoch):
    t = datetime.fromtimestamp(epoch, timezone.utc)
    return t.strftime("%Y-%m-%d") if (t.hour, t.minute, t.second) == (0, 0, 0) else t.strftime("%Y-%m-%d %H:%M UTC")


def money(value, positive=False):
    """A finite non-negative JSON number (above zero when positive)."""
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
        return None
    if value == 0:
        return None if positive else 0.0
    return float(value)


def text(value):
    return ''.join(c for c in value if c.isprintable())[:120] if isinstance(value, str) else None


def grant(raw):
    """One validated grant, or None."""
    if not isinstance(raw, dict):
        return None
    provider, ref, usage_ref = raw.get("provider"), raw.get("credential_ref"), raw.get("usage_credential_ref")
    amount, balance = money(raw.get("amount_usd"), True), money(raw.get("observed_balance_usd"))
    reserve = money(raw["reserve_usd"]) if raw.get("reserve_usd") is not None else 0.0
    received, expires, observed = (instant(raw.get(k)) for k in ("received_at", "expires_at", "observed_at"))
    if (not isinstance(provider, str) or not PROVIDER.fullmatch(provider) or not isinstance(ref, str)
            or not ENV_NAME.fullmatch(ref) or None in (amount, balance, reserve, received, expires, observed)
            or expires <= received
            or (usage_ref is not None and (not isinstance(usage_ref, str) or not ENV_NAME.fullmatch(usage_ref)))
            or any(raw.get(k) is not None and not isinstance(raw[k], str) for k in ("observed_from", "renews"))):
        return None
    return {"provider": provider, "credential_ref": ref, "amount": amount, "balance": balance,
            "reserve": reserve, "received": received, "expires": expires, "observed": observed,
            "usage_ref": usage_ref, "observed_from": text(raw.get("observed_from")), "renews": text(raw.get("renews"))}


def parse(data):
    if not isinstance(data, dict) or data.get("schema") != SCHEMA or not isinstance(data.get("grants"), list):
        raise GrantError("credit_grants_invalid")
    grants = [grant(raw) for raw in data["grants"]]
    if None in grants:
        raise GrantError("credit_grants_invalid")
    return grants


def read(path):
    """The grants file, or None when absent. Never follows a link."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    except OSError:
        raise GrantError("credit_grants_unreadable") from None
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022 or info.st_size > LIMIT:
            raise GrantError("credit_grants_unreadable")
        with os.fdopen(fd, "rb") as stream:
            fd = -1
            raw = stream.read(LIMIT + 1)
    except OSError:
        raise GrantError("credit_grants_unreadable") from None
    finally:
        if fd >= 0:
            os.close(fd)
    try:
        if len(raw) > LIMIT:
            raise ValueError
        return json.loads(raw)
    except (ValueError, UnicodeDecodeError, RecursionError):
        raise GrantError("credit_grants_invalid") from None


def registry_path():
    return os.environ.get("SB_PROVIDER_REGISTRY") or str(Path(__file__).resolve().parents[1] / "config/provider-registry.json")


def price_table(path):
    """(provider, model) -> micro-USD per 1M tokens: (input, output, cached input)."""
    try:
        with open(path, encoding="utf-8") as stream:
            models = json.load(stream).get("models")
    except (OSError, ValueError, RecursionError, AttributeError):
        raise GrantError("price_registry_unavailable") from None
    if not isinstance(models, list):
        raise GrantError("price_registry_unavailable")
    table = {}
    for m in models:
        if isinstance(m, dict) and isinstance(m.get("provider_id"), str) and isinstance(m.get("model_id"), str):
            table.setdefault((m["provider_id"], m["model_id"]), tuple(money(m.get(k)) for k in (
                "input_micros_per_mtok", "output_micros_per_mtok", "cached_input_micros_per_mtok")))
    return table


def rates(table, provider, model):
    model = model if isinstance(model, str) else ""
    for name in (model, model[len(provider) + 1:] if model.startswith(provider + "/") else None,
                 re.sub(r"-\d{4}-\d{2}-\d{2}$", "", model)):
        found = table.get((provider, name))
        if found and found[0] is not None and found[1] is not None:
            return found
    return None


def spend(store, provider, since_ms):
    """Token totals per model since the reading; created_at is in milliseconds."""
    try:
        db = sqlite3.connect(Path(store).absolute().as_uri() + "?mode=ro", uri=True, timeout=0.5)
        try:
            db.execute("PRAGMA query_only = ON")
            return db.execute(
                "SELECT model, COUNT(*), SUM(COALESCE(input_tokens, 0)), SUM(COALESCE(output_tokens, 0)),"
                " SUM(COALESCE(cached_input_tokens, 0)) FROM usage"
                " WHERE provider_id = ? AND created_at >= ? GROUP BY model", (provider, since_ms)).fetchall()
        finally:
            db.close()
    except sqlite3.Error:
        raise GrantError("usage_store_unavailable") from None


def estimate(g, store, registry):
    """(USD spent since the reading, requests with no known price)."""
    totals = spend(store, g["provider"], int(round(g["observed"] * 1000)))
    table = price_table(registry) if totals else {}
    spent, unpriced = 0, 0
    for model, count, inp, out, cached in totals:
        found = rates(table, g["provider"], model)
        if found is None:
            unpriced += count
            continue
        inp, out, cached = (max(0, int(x or 0)) for x in (inp, out, cached))
        spent += inp * found[0] + out * found[1] + cached * (found[2] if found[2] is not None else found[0])
    return spent / 1e12, unpriced


def billed_fetch(g, token, timeout=6):
    """USD the provider billed since the grant date: one bounded fixed-host read."""
    from provider_account_usage import https_json, json_number, UsageError
    host, path = BILLING[g["provider"]]
    try:
        payload = https_json(host, f"{path}?start_time={int(g['received'])}&bucket_width=1d&limit=180", token, timeout)
    except UsageError as failure:
        raise GrantError("billing_" + str(failure)) from None
    buckets = payload.get("data")
    # A further page would mean an undercount; a grant period fits in one.
    if not isinstance(buckets, list) or payload.get("has_more") is not False:
        raise GrantError("billing_invalid_response")
    total = 0.0
    for bucket in buckets:
        results = bucket.get("results") if isinstance(bucket, dict) else None
        if not isinstance(results, list):
            raise GrantError("billing_invalid_response")
        for result in results:
            amount = result.get("amount") if isinstance(result, dict) else None
            value = json_number(amount.get("value")) if isinstance(amount, dict) else None
            if value is None or str(amount.get("currency")).lower() != "usd":
                raise GrantError("billing_invalid_response")
            total += value
    return max(0.0, total)


def billed(g, slot, runtime, now, environment=None, fetch=billed_fetch):
    """None without a usage-read key; else (USD billed, read epoch) or a fixed failure code.

    Successes are cached ten minutes and failures two, so `sb limits` stays quick
    and an offline machine does not wait on every call. The cache holds no secret.
    """
    environment = os.environ if environment is None else environment
    token = environment.get(g["usage_ref"]) if g["usage_ref"] else None
    token = token.strip() if isinstance(token, str) else ""
    if g["provider"] not in BILLING or not token:
        return None
    path, key = Path(runtime) / "state/credit-grant-billing.json", f"{slot}@{int(g['received'])}"
    try:
        with open(path, encoding="utf-8") as stream:
            cache = json.load(stream)
    except (OSError, ValueError, RecursionError):
        cache = {}
    cache = {k: v for k, v in cache.items() if isinstance(v, dict) and money(v.get("read_at")) is not None
             and 0 <= now - v["read_at"] < BILLING_KEEP} if isinstance(cache, dict) else {}
    entry = cache.get(key)
    if entry and isinstance(entry.get("error"), str) and now - entry["read_at"] < BILLING_RETRY:
        return entry["error"][:64]
    if entry and money(entry.get("billed_usd")) is not None and now - entry["read_at"] < BILLING_TTL:
        return float(entry["billed_usd"]), float(entry["read_at"])
    try:
        outcome = (fetch(g, token), now)
        cache[key] = {"billed_usd": outcome[0], "read_at": now}
    except GrantError as failure:
        outcome = str(failure)
        cache[key] = {"error": outcome, "read_at": now}
    try:
        fd, name = tempfile.mkstemp(prefix=".credit-grant-billing-", dir=path.parent)
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                json.dump(cache, stream)
            os.replace(name, path)
        finally:
            if os.path.exists(name):
                os.unlink(name)
    except OSError:
        pass  # A reading that cannot be cached is still a reading.
    return outcome


def grant_slot(provider, ordinal):
    from provider_account_usage import slot_name
    return slot_name(provider, "credit-grant" if ordinal == 1 else f"credit-grant-{ordinal}")


def row(g, ordinal, now, store, registry, billing=None):
    from native_account_usage import window
    p, amount, reserve = g["provider"], g["amount"], g["reserve"]
    shown = ROW_PROVIDER.get(p, p)
    slot = grant_slot(shown, ordinal)
    error, spent, unpriced = None, None, None
    exact = isinstance(billing, tuple)
    elapsed = min(1.0, max(0.0, (now - g["received"]) / (g["expires"] - g["received"])))
    days_left = max(0.0, (g["expires"] - now) / 86400)
    if now >= g["expires"]:
        # Unused credit is gone at expiry; the store is not needed to know that.
        balance, used, per_day, state, elapsed = 0.0, 1.0, 0.0, "expired", 1.0
        notes = [f"Credit expired on {day(g['expires'])}; nothing is left. Record the new grant: "
                 f"sb credits observe {p} --balance USD --expires YYYY-MM-DD"]
    else:
        try:
            spent, unpriced = estimate(g, store, registry)
        except GrantError as failure:
            error = str(failure)
        if exact:
            # The provider's billing sees every call; the gateway estimate is then only extra detail.
            error = None
        if error:
            # Unknown is not zero: never present the last reading as the current balance.
            balance = used = per_day = None
            state = "unknown"
            notes = [f"Balance unknown: {UNKNOWN[error]}. Last true reading ${g['balance']:.2f} at "
                     f"{iso(g['observed'])}; spending since then is not known."]
        else:
            balance = max(0.0, amount - billing[0]) if exact else max(0.0, g["balance"] - spent)
            used = min(1.0, max(0.0, (amount - balance) / amount))
            per_day = max(0.0, balance - reserve) / max(days_left, 1.0)
            state = "over" if used > elapsed + PACE_BAND else "under" if used < elapsed - PACE_BAND else "on_pace"
            keep = f", keeping ${reserve:.2f} back" if reserve else ""
            lead = f"${balance:.2f} of ${amount:.2f} left by the provider's billing" if exact else (
                f"Estimated ${balance:.2f} of ${amount:.2f} left")
            notes = [f"{lead}, expires {day(g['expires'])} "
                     f"({days_left:.1f} days). May spend about ${per_day:.2f} a day{keep}. "
                     f"Spending is {state.replace('_', ' ')}{'' if state == 'on_pace' else ' pace'}."]
            if isinstance(billing, str):
                notes.append(f"The provider's billing could not be read ({billing}); this is the gateway estimate.")
            if unpriced and not exact:
                notes.append(f"{unpriced} request{' had' if unpriced == 1 else 's had'} no known price and "
                             f"{'is' if unpriced == 1 else 'are'} not in the estimate.")
    sampled = None if error else billing[1] if exact and now < g["expires"] else now
    w = {**window("credit_grant", None if used is None else round(100 * used, 4), g["expires"], sampled, now,
                  (g["expires"] - g["received"]) / 60), "scope": "generation", "estimated": not exact}
    facts = {k: g[k] for k in ("provider", "credential_ref", "amount", "received", "expires", "observed", "balance")}
    balance_row = {"kind": "prepaid", "balance": None if balance is None else round(balance, 8),
                   "currencyCode": "USD", "period": f"{shown} credit, expires {day(g['expires'])}",
                   "estimated": not exact, "observedBalance": g["balance"], "observedAt": iso(g["observed"]),
                   "spentSinceObserved": None if spent is None else round(spent, 8),
                   "unpricedRequests": unpriced, "grantAmount": amount, "basis": BILLING_BASIS if exact else BASIS}
    if exact:
        balance_row.update(billedSinceGrant=round(billing[0], 8), billingReadAt=iso(billing[1]))
    if g["observed_from"]:
        balance_row["observedFrom"] = g["observed_from"]
    result = {"provider": shown, "slot": slot, "source": "credit-grant/" + p, "credential_ref": g["credential_ref"],
              "account_label": "credit grant",
              "account_id": {"kind": "credit_grant", "value": slot,
                             "revision": hashlib.sha256(json.dumps(facts, sort_keys=True).encode()).hexdigest()},
              "windows": [], "grant_window": w, "notes": notes, "balances": [balance_row],
              "observed_at": None if sampled is None else iso(sampled), "relogin_required": False,
              "estimated": not exact, "state": "unknown" if sampled is None else "fresh",
              "pace": {"state": state, "days_left": round(days_left, 3),
                       "allowed_per_day_usd": None if per_day is None else round(per_day, 4),
                       "used_fraction": None if used is None else round(used, 6),
                       "elapsed_fraction": round(elapsed, 6), "reserve_usd": reserve,
                       "expires_at": iso(g["expires"])}}
    if g["renews"]:
        result["renews"] = g["renews"]
    if error:
        result["refresh_error"] = error
    return result


def view(runtime, now=None, registry=None, environment=None, fetch=billed_fetch):
    """(one account row per grant, fixed diagnostic or None). A missing file is no rows."""
    now = time.time() if now is None else now
    try:
        data = read(Path(runtime) / "config/credit-grants.json")
        if data is None:
            return [], None
        grants = parse(data)
    except GrantError as failure:
        return [], str(failure)
    seen, rows = {}, []
    for g in grants:
        seen[g["provider"]] = seen.get(g["provider"], 0) + 1
        ordinal = seen[g["provider"]]
        slot = grant_slot(ROW_PROVIDER.get(g["provider"], g["provider"]), ordinal)
        billing = billed(g, slot, runtime, now, environment, fetch) if now < g["expires"] else None
        rows.append(row(g, ordinal, now, Path(runtime) / "state/scout.sqlite", registry or registry_path(), billing))
    return rows, None


def attach(result, runtime, provider=None, slot=None, now=None, registry=None):
    """Append grant rows to an `sb limits` snapshot after its own rows; never touches them."""
    rows, problem = view(runtime, now, registry)
    if problem:
        result["coverage"].append({"provider": "credit-grants", "state": "unavailable", "reason": problem})
    result["accounts"] += [r for r in rows if provider in (None, "all", r["provider"]) and slot in (None, r["slot"])]
    return result


def render(rows, problem):
    if problem:
        print(f"Credit grants unavailable ({problem}); fix or re-record credit-grants.json.")
    elif not rows:
        print("No credit grants recorded. Record one: sb credits observe PROVIDER --balance USD --expires YYYY-MM-DD")
    for r in rows:
        b = r["balances"][0]
        kind = "estimate" if r["estimated"] else "billing read"
        print(f'{r["provider"]}/{r["slot"]} [{r["state"]}; {r["source"]}; {kind} at {r.get("observed_at") or "unknown"}]')
        for note in r["notes"]:
            print("  " + note)
        if b["spentSinceObserved"] is not None:
            source = f' ({b["observedFrom"]})' if b.get("observedFrom") else ""
            print(f'  Last true reading ${b["observedBalance"]:.2f} at {b["observedAt"]}{source}; '
                  f'gateway spend since: ${b["spentSinceObserved"]:.4f}.')
    if any(r["estimated"] for r in rows):
        print("Estimates use list prices, and calls that do not pass through the gateway are not seen.")


def fail(message, code=2):
    print("sb credits: " + message, file=sys.stderr)
    return code


def observe(argv, runtime, now=None):
    """Record a fresh true reading for one provider's grant; on bad input nothing changes."""
    now = time.time() if now is None else now
    if not argv or not PROVIDER.fullmatch(argv[0]):
        return fail("invalid provider id; nothing changed")
    provider, options, rest = argv[0], {}, argv[1:]
    names = {"--balance", "--expires", "--received", "--amount", "--reserve"}
    while rest:
        if len(rest) < 2 or rest[0] not in names or rest[0] in options:
            return fail(USAGE)
        options[rest[0]], rest = rest[1], rest[2:]
    if "--balance" not in options or "--expires" not in options:
        return fail(USAGE)
    numbers = {}
    for key in ("--balance", "--amount", "--reserve"):
        if key in options:
            try:
                numbers[key] = money(float(options[key]), key == "--amount")
            except (ValueError, OverflowError):
                numbers[key] = None
            if numbers[key] is None:
                return fail("amounts must be finite non-negative numbers (a grant amount above zero); nothing changed")
    if any(instant(options[k]) is None for k in ("--expires", "--received") if k in options):
        return fail("dates must be YYYY-MM-DD or ISO timestamps; nothing changed")
    path = Path(runtime) / "config/credit-grants.json"
    try:
        info = os.lstat(path.parent)
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
            raise OSError
    except OSError:
        return fail("runtime config directory is unavailable; nothing changed", 1)
    try:
        data = read(path) or {"schema": SCHEMA, "grants": []}
        parse(data)
    except GrantError:
        return fail("credit-grants.json is unreadable or invalid; nothing changed", 1)
    matches = [i for i, raw in enumerate(data["grants"]) if raw.get("provider") == provider]
    if len(matches) > 1:
        return fail("more than one grant for this provider; edit credit-grants.json by hand", 1)
    raw = dict(data["grants"][matches[0]]) if matches else {
        "provider": provider, "credential_ref": provider.upper().replace("-", "_") + "_API_KEY",
        "amount_usd": numbers["--balance"], "received_at": datetime.fromtimestamp(now, timezone.utc).strftime("%Y-%m-%d")}
    raw.update(observed_balance_usd=numbers["--balance"], observed_at=iso(int(now)), expires_at=options["--expires"])
    for key, field in (("--received", "received_at"), ("--amount", "amount_usd"), ("--reserve", "reserve_usd")):
        if key in options:
            raw[field] = numbers.get(key, options[key])
    received = instant(raw.get("received_at"))
    if received is not None and instant(raw["expires_at"]) <= received:
        return fail("expiry must be after the received date; nothing changed")
    if grant(raw) is None:
        return fail("invalid credit reading; nothing changed")
    grants = list(data["grants"])
    if matches:
        grants[matches[0]] = raw
    else:
        grants.append(raw)
    data = {**data, "grants": grants}
    parse(data)
    fd, name = tempfile.mkstemp(prefix=".credit-grants-", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(data, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(name):
            os.unlink(name)
    print(f"Recorded {provider} credit reading: ${numbers['--balance']:.2f} at {raw['observed_at']}, "
          f"expires {day(instant(raw['expires_at']))}.")
    return 0


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] in (["-h"], ["--help"], ["help"]):
        print(USAGE)
        return 0
    runtime = os.environ.get("SWITCHBACK_RUNTIME_ROOT", "")
    if not os.path.isabs(runtime):
        return fail("an absolute owned runtime is required", 1)
    if argv[:1] == ["observe"]:
        return observe(argv[1:], runtime)
    if argv not in ([], ["--json"]):
        return fail(USAGE)
    rows, problem = view(runtime)
    if argv:
        print(json.dumps({"schema": VIEW_SCHEMA, "generated_at": iso(time.time()), "accounts": rows,
                          "coverage": [{"provider": "credit-grants", "state": "unavailable", "reason": problem}]
                          if problem else [], "purpose": "estimate_not_a_bill"}))
    else:
        render(rows, problem)
    return 0


if __name__ == "__main__":
    sys.exit(main())
