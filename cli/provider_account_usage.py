"""Direct quota and balance observations. Credentials remain in memory with their existing owner.

Provider wire facts were checked against CodexBar's MIT-licensed collectors;
this module is an independent implementation, with no credential-store imports.
"""
from datetime import datetime
import hashlib
import http.client
import json
import math
import os
import re
import socket
import ssl
import time
from urllib.parse import urlsplit

FAMILIES = {"zai": "zai", "zai-coding": "zai", "zai-coding-headroom": "zai",
            "minimax": "minimax", "minimax-coding": "minimax",
            "deepseek": "deepseek", "openrouter": "openrouter",
            "neuralwatt": "neuralwatt", "neuralwatt-coding": "neuralwatt"}
DEFAULT_ENV = {"zai": "ZAI_API_KEY", "minimax": "MINIMAX_API_KEY", "deepseek": "DEEPSEEK_API_KEY",
               "openrouter": "OPENROUTER_API_KEY", "neuralwatt": "NEURALWATT_API_KEY"}
# Every family here is collected natively; callers must not also bridge it.
DIRECT_FAMILIES = frozenset(DEFAULT_ENV)
HOSTS = {"zai": {"global": "api.z.ai", "cn": "open.bigmodel.cn"},
         "minimax": {"global": "api.minimax.io", "cn": "api.minimaxi.com"},
         "deepseek": {"global": "api.deepseek.com"},
         "openrouter": {"global": "openrouter.ai"},
         "neuralwatt": {"global": "api.neuralwatt.com"},
         # Billing-only host for credit grants (credit_grants.py); no usage collector is discovered for it.
         "openai": {"global": "api.openai.com"}}
ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
MAX_BODY = 131072


class UsageError(Exception):
    """A fixed nonsecret diagnostic, never an upstream body or exception text."""


def numeric(value):
    if isinstance(value, bool):
        return None
    try:
        value = float(value)
        return value if math.isfinite(value) else None
    except (ValueError, TypeError, OverflowError):
        return None


def percent(value):
    value = numeric(value)
    return min(100, max(0, value)) if value is not None else None


def https_json(host, path, token, timeout):
    """Fixed-host TLS, no redirect/proxy/cookie storage, bounded body and deadline."""
    if host not in {h for regions in HOSTS.values() for h in regions.values()}:
        raise UsageError("unsupported_endpoint")
    deadline = time.monotonic() + timeout
    connection = http.client.HTTPSConnection(host, timeout=timeout, context=ssl.create_default_context())
    try:
        connection.connect()
        request_socket = connection.sock
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise UsageError("timeout")
        request_socket.settimeout(remaining)
        connection.request("GET", path, headers={
            "Authorization": "Bearer " + token, "Accept": "application/json",
            "Content-Type": "application/json", "MM-API-Source": "Switchback"})
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise UsageError("timeout")
        request_socket.settimeout(remaining)
        response = connection.getresponse()
        status = response.status
        if status in (401, 403):
            raise UsageError("authentication_required")
        if status == 429:
            raise UsageError("rate_limited")
        if status in (404, 405):
            raise UsageError("endpoint_unavailable")
        if status != 200:
            raise UsageError("source_error")
        length = response.getheader("Content-Length")
        if length is not None:
            try:
                if int(length) < 0 or int(length) > MAX_BODY:
                    raise UsageError("response_too_large")
            except ValueError:
                raise UsageError("invalid_response") from None
        chunks, size = [], 0
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise UsageError("timeout")
            request_socket.settimeout(remaining)
            chunk = response.read1(min(16384, MAX_BODY + 1 - size))
            if not chunk:
                break
            size += len(chunk)
            if size > MAX_BODY:
                raise UsageError("response_too_large")
            chunks.append(chunk)
        try:
            payload = json.loads(b"".join(chunks))
        except (ValueError, UnicodeError):
            raise UsageError("invalid_response") from None
        if not isinstance(payload, dict):
            raise UsageError("invalid_response")
        return payload
    except UsageError:
        raise
    except (socket.timeout, TimeoutError):
        raise UsageError("timeout") from None
    except (OSError, http.client.HTTPException, ValueError):
        raise UsageError("source_unavailable") from None
    finally:
        connection.close()


def epoch(value):
    value = numeric(value)
    if value is None or value <= 1_000_000_000:
        return None
    return value / 1000 if value > 1_000_000_000_000 else value


def parse_zai(payload, now, make_window):
    if payload.get("success") is not True or payload.get("code") != 200:
        raise UsageError("source_error")
    data = payload.get("data")
    limits = data.get("limits") if isinstance(data, dict) else None
    if not isinstance(limits, list):
        raise UsageError("invalid_response")
    windows = []
    for raw in limits:
        if not isinstance(raw, dict) or raw.get("type") not in ("TOKENS_LIMIT", "TIME_LIMIT"):
            continue
        kind = raw["type"]
        total, used, left = (numeric(raw.get(k)) for k in ("usage", "currentValue", "remaining"))
        candidates = [x for x in (used, total - left if total is not None and left is not None else None)
                      if x is not None]
        used_percent = percent(100 * max(candidates) / total) if total and total > 0 and candidates else percent(raw.get("percentage"))
        count = numeric(raw.get("number"))
        unit = raw.get("unit")
        multiplier = {1: 1440, 3: 60, 5: 1, 6: 10080}.get(unit) if isinstance(unit, int) and not isinstance(unit, bool) else None
        minutes = count * multiplier if count and count > 0 and multiplier else None
        if kind == "TIME_LIMIT":
            name, minutes = "mcp_monthly", None
        else:
            name = "tokens_" + (f"{minutes:g}m" if minutes else "unknown")
        reset = numeric(raw.get("nextResetTime"))
        reset = reset / 1000 if reset is not None else None
        w = make_window(name, used_percent, reset, now, now, minutes)
        w["scope"] = "tools" if kind == "TIME_LIMIT" else "generation"
        windows.append(w)
    if not windows:
        raise UsageError("no_usage_source")
    return windows, []


def minimax_reset(raw, prefix):
    # Untyped relative durations cannot establish a reset timestamp.
    return epoch(raw.get("weekly_end_time" if prefix == "weekly" else "end_time"))


def parse_minimax(payload, now, make_window):
    data = payload.get("data", payload)
    if not isinstance(data, dict):
        raise UsageError("invalid_response")
    base = data.get("base_resp", payload.get("base_resp", {}))
    if not isinstance(base, dict):
        raise UsageError("invalid_response")
    status = numeric(base.get("status_code", 0))
    if status == 1004:
        raise UsageError("authentication_required")
    if status != 0:
        raise UsageError("source_error")
    models = data.get("model_remains")
    if not isinstance(models, list):
        raise UsageError("unsupported_response_shape")
    windows, notes = [], []
    for index, raw in enumerate(models):
        if not isinstance(raw, dict):
            continue
        model = raw.get("model_name")
        model = ''.join(c for c in model if c.isprintable())[:80] if isinstance(model, str) else f"model_{index + 1}"
        for prefix in ("interval", "weekly"):
            field = "current_" + prefix + "_"
            total, left, remaining_percent, status = (numeric(raw.get(field + suffix)) for suffix in
                                                     ("total_count", "usage_count", "remaining_percent", "status"))
            if status == 3 and remaining_percent is not None and remaining_percent >= 100:
                if prefix == "weekly" and model.lower() in ("general", "text generation", "text-generation"):
                    notes.append(model + ": weekly unlimited")
                    continue
                if not total and not left:
                    continue
            used = percent(100 - remaining_percent) if remaining_percent is not None else (
                percent(100 * (total - left) / total) if total and total > 0 and left is not None else None)
            if used is None:
                continue
            start = epoch(raw.get("weekly_start_time" if prefix == "weekly" else "start_time"))
            end = epoch(raw.get("weekly_end_time" if prefix == "weekly" else "end_time"))
            minutes = (end - start) / 60 if end is not None and start is not None and end > start else None
            reset = minimax_reset(raw, prefix)
            w = make_window(model + "/" + prefix, used, reset, now, now, minutes)
            w["scope"] = "generation"
            w["model"] = model
            if reset is None:
                w["reset_source"] = "unavailable"
            windows.append(w)
    if not windows and not notes:
        raise UsageError("no_usage_source")
    return windows, notes


# Balance-only collectors return (windows, notes, balances). A balance row uses
# the same keys the menu bar reads: kind, balance, currencyCode, period.
def json_number(value):
    """A finite JSON number; strings and booleans are malformed here."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value if math.isfinite(value) else None


def parse_deepseek(payload, now, make_window):
    infos = payload.get("balance_infos")
    if not isinstance(infos, list) or not isinstance(payload.get("is_available"), bool):
        raise UsageError("invalid_response")
    rows = []
    for raw in infos:
        if not isinstance(raw, dict):
            raise UsageError("invalid_response")
        currency = raw.get("currency")
        # DeepSeek sends decimal strings; anything non-numeric is malformed, not zero.
        amounts = [numeric(raw.get(k)) for k in ("total_balance", "granted_balance", "topped_up_balance")]
        if not isinstance(currency, str) or not re.fullmatch(r"[A-Z]{3}", currency) or None in amounts:
            raise UsageError("invalid_response")
        rows.append({"kind": "prepaid", "balance": amounts[0], "currencyCode": currency,
                     "period": "DeepSeek balance", "grantedBalance": amounts[1],
                     "toppedUpBalance": amounts[2]})
    if not rows:
        raise UsageError("no_usage_source")
    # Show every funded wallet; when none is funded, one measured zero (USD first).
    balances = [b for b in rows if b["balance"] > 0] or [
        next((b for b in rows if b["currencyCode"] == "USD"), rows[0])]
    notes = [] if payload["is_available"] else ["DeepSeek reports the balance cannot cover API calls."]
    return [], notes, balances


def parse_openrouter(payload, now, make_window):
    data = payload.get("data")
    data = data if isinstance(data, dict) else {}
    credits, used = json_number(data.get("total_credits")), json_number(data.get("total_usage"))
    if credits is None or used is None or credits < 0 or used < 0:
        raise UsageError("invalid_response")
    return [], [], [{"kind": "prepaid", "balance": round(max(0.0, credits - used), 8),
                     "currencyCode": "USD", "period": "OpenRouter credits",
                     "totalCredits": credits, "totalUsage": used}]


def iso_epoch(value):
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return parsed.timestamp() if parsed.tzinfo else None
    except (ValueError, TypeError, AttributeError, OverflowError):
        return None


def parse_neuralwatt(payload, now, make_window):
    balance = payload.get("balance")
    if not isinstance(balance, dict):
        raise UsageError("invalid_response")
    fields = ("credits_remaining_usd", "total_credits_usd", "credits_used_usd")
    if any(balance.get(k) is not None and json_number(balance.get(k)) is None for k in fields):
        raise UsageError("invalid_response")
    remaining, total, spent = (json_number(balance.get(k)) for k in fields)
    if remaining is None and total is not None and total > 0 and spent is not None and spent >= 0:
        remaining = max(0.0, total - spent)
    if remaining is None or remaining < 0:
        raise UsageError("invalid_response")
    windows, notes = [], []
    # Optional subscription kWh allowance; null means prepaid-only.
    sub = payload.get("subscription")
    if isinstance(sub, dict):
        included, kwh_used, kwh_left = (json_number(sub.get(k)) for k in ("kwh_included", "kwh_used", "kwh_remaining"))
        if not included and kwh_used is not None and kwh_left is not None:
            included = kwh_used + kwh_left
        if kwh_used is None and included and kwh_left is not None:
            kwh_used = max(0.0, included - kwh_left)
        if included and included > 0 and kwh_used is not None and kwh_used >= 0:
            start, end = iso_epoch(sub.get("current_period_start")), iso_epoch(sub.get("current_period_end"))
            minutes = (end - start) / 60 if start is not None and end is not None and end > start else None
            w = make_window("subscription_kwh", percent(100 * kwh_used / included), end, now, now, minutes)
            w["scope"] = "generation"
            windows.append(w)
            notes.append(f"{kwh_used:g} / {included:g} kWh this period")
    return windows, notes, [{"kind": "prepaid", "balance": remaining, "currencyCode": "USD",
                             "period": "Neuralwatt prepaid balance"}]


BALANCE_ENDPOINTS = {"deepseek": ("/user/balance", parse_deepseek),
                     "openrouter": ("/api/v1/credits", parse_openrouter),
                     "neuralwatt": ("/v1/quota", parse_neuralwatt)}


def slot_name(provider_id, account_id):
    raw = provider_id + "--" + account_id
    if re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", raw):
        return raw
    slug = re.sub(r"[^A-Za-z0-9_-]+", "-", raw).strip("-_") or "account"
    # The full original identity participates, even when labels share a prefix.
    return slug[:47] + "-" + hashlib.sha256(raw.encode()).hexdigest()[:16]


def discover(definitions, families, environment=None):
    """Resolve only explicit env-backed accounts; never load a secret store."""
    environment = os.environ if environment is None else environment
    definitions = definitions if isinstance(definitions, list) else []
    accounts, represented, subscriptions = [], set(), set()
    # Prefer the primary route as the stable label when aliases share a key.
    preference = {"zai-coding": 0, "minimax-coding": 0, "neuralwatt-coding": 0,
                  "zai": 1, "minimax": 1, "neuralwatt": 1}
    definitions = sorted(definitions, key=lambda d: preference.get(d.get("id"), 2)
                         if isinstance(d, dict) and isinstance(d.get("id"), str) else 3)
    for definition in definitions:
        if not isinstance(definition, dict):
            continue
        provider_id = definition.get("id")
        family = FAMILIES.get(provider_id) if isinstance(provider_id, str) else None
        if family not in families:
            continue
        represented.add(family)
        configured = definition.get("accounts")
        configured = configured if isinstance(configured, list) and configured else [
            {"id": "configured", "auth": {"kind": "api_key", "env": definition.get("api_key_env") or DEFAULT_ENV[family]}}]
        for index, account in enumerate(configured):
            if not isinstance(account, dict):
                continue
            account_id = account.get("id")
            account_id = account_id if isinstance(account_id, str) and account_id else f"account-{index + 1}"
            auth = account.get("auth")
            auth = auth if isinstance(auth, dict) else {}
            ref = auth.get("env")
            supported = auth.get("kind") == "api_key" and not auth.get("vault") and not auth.get("inline") and isinstance(ref, str) and ENV_NAME.fullmatch(ref)
            token = environment.get(ref) if supported else None
            token = token.strip() if isinstance(token, str) else None
            region = environment.get("SB_" + family.upper() + "_USAGE_REGION")
            if region is None:
                base_host = urlsplit(definition.get("base_url") or "").hostname
                cn_hosts = {HOSTS[family].get("cn"), "platform.minimaxi.com"} - {None}
                region = "cn" if base_host in cn_hosts else "global"
            error = None if supported and token else ("credential_unavailable" if supported else "credential_source_unsupported")
            if region not in HOSTS[family]:
                error = "unsupported_region"
            revision = hashlib.sha256((provider_id + "\0" + account_id + "\0" + str(ref) + "\0" + (token or "") + "\0" + str(region)).encode()).hexdigest()
            if token and not error:
                subscription = (family, hashlib.sha256(token.encode()).hexdigest(), region)
                if subscription in subscriptions:
                    continue
                subscriptions.add(subscription)
            accounts.append({"provider": family, "slot": slot_name(provider_id, account_id),
                             "account_label": ''.join(c for c in account_id if c.isprintable())[:240],
                             "source": "native/" + family, "credential_ref": ref if supported else None,
                             "credential_revision": revision, "region": region,
                             "_token": token, "_error": error})
    for family in sorted(set(families) & set(DEFAULT_ENV) - represented):
        ref = DEFAULT_ENV[family]
        if family == "zai" and not environment.get(ref) and environment.get("Z_AI_API_KEY"):
            ref = "Z_AI_API_KEY"
        accounts.extend(discover([{"id": family, "api_key_env": ref}], {family}, environment))
    return accounts


def collect(account, timeout, now, make_window, transport=https_json):
    error = account.get("_error")
    if error:
        raise UsageError(error)
    family, region, token = account["provider"], account["region"], account["_token"]
    host = HOSTS[family][region]
    deadline = time.monotonic() + timeout
    if family in BALANCE_ENDPOINTS:
        path, parse = BALANCE_ENDPOINTS[family]
        return parse(transport(host, path, token, timeout), now, make_window)
    if family == "zai":
        payload = transport(host, "/api/monitor/usage/quota/limit", token, timeout)
        return parse_zai(payload, now, make_window)
    try:
        payload = transport(host, "/v1/token_plan/remains", token, timeout)
        return parse_minimax(payload, now, make_window)
    except UsageError as first_error:
        # Older coding-plan keys may be rejected by the newer token-plan API.
        # Keep the exact account/token/region; only the same-host path changes.
        if str(first_error) not in ("endpoint_unavailable", "authentication_required",
                                    "invalid_response", "unsupported_response_shape",
                                    "no_usage_source", "source_unavailable"):
            raise
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise UsageError("timeout") from None
        try:
            payload = transport(host, "/v1/api/openplatform/coding_plan/remains", token, remaining)
            return parse_minimax(payload, now, make_window)
        except UsageError:
            if str(first_error) == "authentication_required":
                raise UsageError("authentication_required") from None
            raise
