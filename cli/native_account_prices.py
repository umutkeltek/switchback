"""Pinned API list prices for estimating what local Claude and Codex logs would cost.

These accounts are subscriptions. The number is an estimate: what the same
tokens would cost at the public API list price, never a bill.

Prices are USD per million tokens, Standard tier, global routing.
Read on 2026-11-17 from:
  https://platform.claude.com/docs/en/about-claude/pricing
    (model table: input, output, 5-minute cache write, 1-hour cache write,
     cache hit; fast mode; inference_geo "us" = 1.1x for 4.6 and later;
     Haiku 5.5 prompts over 100,000 tokens use the higher row)
  https://developers.openai.com/api/docs/pricing and the model pages
  https://developers.openai.com/api/docs/models/<model>
    (input, cached input, 30-minute cache write, output; prompts over
     272K input tokens pay 2x input and cache rates and 1.5x output)

The provider registry (config/provider-registry.json) has no cache-write
fields, so the table lives here. A model missing from this table is never
priced: its tokens are counted and reported as unpriced.
"""
import re

PRICES_VERSION = "2026-11-17.1"

# input, output, cache write 5m, cache write 1h, cache read (USD / MTok)
_FABLE_5_2 = (8.0, 40.0, 10.0, 16.0, 0.20)
_FABLE_5_1 = (10.0, 50.0, 12.5, 20.0, 0.25)
_FABLE_5 = (10.0, 50.0, 12.5, 20.0, 1.0)
_OPUS_5 = (5.0, 25.0, 6.25, 10.0, 0.50)
_OPUS_4_1 = (15.0, 75.0, 18.75, 30.0, 1.50)
_SONNET_4 = (3.0, 15.0, 3.75, 6.0, 0.30)

CLAUDE = {
    "claude-fable-5-2": {"rates": _FABLE_5_2, "fast": 2.0, "geo": True},
    "claude-mythos-5-2": {"rates": _FABLE_5_2, "geo": True},
    "claude-opus-5-5": {"rates": (4.0, 20.0, 5.0, 8.0, 0.20), "fast": 2.0, "geo": True},
    "claude-sonnet-5-5": {"rates": (2.0, 10.0, 2.5, 4.0, 0.10), "geo": True},
    "claude-haiku-5-5": {"rates": (0.10, 0.50, 0.125, 0.20, 0.01), "geo": True,
                         "above": (100_000, (0.50, 2.50, 0.625, 1.0, 0.05))},
    "claude-fable-5-1": {"rates": _FABLE_5_1, "geo": True},
    "claude-mythos-5-1": {"rates": _FABLE_5_1, "geo": True},
    "claude-fable-5": {"rates": _FABLE_5, "geo": True},
    "claude-mythos-5": {"rates": _FABLE_5, "geo": True},
    "claude-opus-5": {"rates": _OPUS_5, "fast": 2.0, "geo": True},
    "claude-opus-4-8": {"rates": _OPUS_5, "fast": 2.0, "geo": True},
    "claude-opus-4-7": {"rates": _OPUS_5, "geo": True},
    "claude-opus-4-6": {"rates": _OPUS_5, "geo": True},
    "claude-opus-4-5": {"rates": _OPUS_5},
    "claude-opus-4-1": {"rates": _OPUS_4_1},
    "claude-opus-4": {"rates": _OPUS_4_1},
    "claude-sonnet-5": {"rates": (2.0, 10.0, 2.5, 4.0, 0.20), "geo": True},
    "claude-sonnet-4-6": {"rates": _SONNET_4, "geo": True},
    "claude-sonnet-4-5": {"rates": _SONNET_4},
    "claude-sonnet-4": {"rates": _SONNET_4},
    "claude-haiku-4-5": {"rates": (1.0, 5.0, 1.25, 2.0, 0.10)},
}

# input, output, cache write (30m), cached input (USD / MTok); 272K long-context rule
CODEX = {
    "gpt-6.2-astra": (8.0, 40.0, 10.0, 0.40),
    "gpt-6.2-sol": (2.0, 10.0, 2.5, 0.10),
    "gpt-6.1-sol": (2.0, 10.0, 2.5, 0.10),
    "gpt-6-astra": (10.0, 50.0, 12.5, 1.00),
    "gpt-6-luna": (0.10, 0.50, 0.125, 0.01),
}
CODEX_LONG_CONTEXT = 272_000


def claude_model(raw):
    """Normalize an id seen in a transcript to a table key (dates and route prefixes dropped)."""
    if not isinstance(raw, str):
        return None
    model = raw.strip()
    if "claude-" in model:
        model = model[model.index("claude-"):]
    model = re.sub(r"-v\d+:\d+$", "", model)
    model = re.sub(r"\[[^\]]*\]$", "", model)
    base = re.sub(r"-\d{8}$", "", model)
    return base if base in CLAUDE else model


def codex_model(raw):
    if not isinstance(raw, str):
        return None
    model = raw.strip()
    if model.startswith("openai/"):
        model = model[len("openai/"):]
    base = re.sub(r"-\d{4}-\d{2}-\d{2}$", "", model)
    return base if base in CODEX else model


def claude_cost_nanos(model, input_tokens, write_5m, write_1h, cache_read, output, speed=None, geo=None):
    """Nano-USD for one Claude request, or None when the model has no pinned price."""
    entry = CLAUDE.get(model)
    if entry is None:
        return None
    rates = entry["rates"]
    above = entry.get("above")
    if above and input_tokens + write_5m + write_1h + cache_read > above[0]:
        rates = above[1]
    factor = 1.0
    if speed == "fast" and entry.get("fast"):
        factor *= entry["fast"]
    if geo == "us" and entry.get("geo"):
        factor *= 1.1
    usd_per_mtok = (input_tokens * rates[0] + output * rates[1] + write_5m * rates[2]
                    + write_1h * rates[3] + cache_read * rates[4])
    return int(round(usd_per_mtok * factor * 1000))


def codex_cost_nanos(model, uncached_input, cache_write, cached_input, output):
    """Nano-USD for one Codex response, or None when the model has no pinned price."""
    rates = CODEX.get(model)
    if rates is None:
        return None
    prompt = uncached_input + cache_write + cached_input
    in_factor, out_factor = (2.0, 1.5) if prompt > CODEX_LONG_CONTEXT else (1.0, 1.0)
    usd_per_mtok = (uncached_input * rates[0] * in_factor + output * rates[1] * out_factor
                    + cache_write * rates[2] * in_factor + cached_input * rates[3] * in_factor)
    return int(round(usd_per_mtok * 1000))
