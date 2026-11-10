"""Environment boundary for vendor-owned native account authentication.

This module selects native homes and removes inherited credential/capture routing.
It never reads credentials, modifies settings, or changes the source environment.
"""
from collections.abc import Mapping
import os
import re

from native_account_process import Refusal

# Exact known overrides: model choices and ordinary CLI preferences remain intact.
OVERRIDES = frozenset((
    "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_URL", "ANTHROPIC_API_BASE", "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_FOUNDRY_API_KEY", "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL", "ANTHROPIC_BEDROCK_BASE_URL",
    "OPENAI_API_KEY", "OPENAI_AUTH_TOKEN", "OPENAI_ACCESS_TOKEN", "OPENAI_TOKEN",
    "OPENAI_BASE_URL", "OPENAI_API_BASE", "OPENAI_API_URL", "CODEX_API_KEY",
    "OPENAI_ORG_ID", "OPENAI_ORGANIZATION", "OPENAI_PROJECT_ID",
    "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES", "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR", "CCR_OAUTH_TOKEN_FILE",
    "CLAUDE_CONFIG_DIR", "CLAUDE_SECURESTORAGE_CONFIG_DIR", "CODEX_HOME",
    "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_NATIVE_DIRECT",
    "SB_NATIVE_ACCOUNT_ATTEMPT", "SB_NATIVE_ACCOUNT_SESSION", "SB_NATIVE_ACCOUNT_SLOT",
))
CONTROL_BYPASS = frozenset((
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NODE_EXTRA_CA_CERTS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
))
HEADERS = (
    "x-switchback-account-attempt",
    "x-switchback-account-session",
    "x-switchback-account-slot",
)
IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")


def _entries(mapping):
    if not isinstance(mapping, Mapping):
        raise Refusal("native environment must be a mapping")
    for key, value in mapping.items():
        if (not isinstance(key, str) or not key or "=" in key or "\0" in key
                or not isinstance(value, str) or "\0" in value):
            raise Refusal("native environment entries must be valid strings")
    return mapping


def _override(key):
    upper = key.upper()
    return upper in OVERRIDES or upper.startswith(("CLAUDE_CODE_USE_", "SB_ACCOUNT_"))


def validate_settings_env(mapping):
    """Raise Refusal when settings could override native account ownership."""
    for key in _entries(mapping):
        if _override(key) or key.upper() == "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":
            raise Refusal("settings env contains a native credential or capture override")


def _capture_headers(mapping):
    if mapping is None:
        return None
    if not isinstance(mapping, Mapping) or len(mapping) > len(HEADERS):
        raise Refusal("invalid native account capture headers")
    normalized = {}
    for key, value in mapping.items():
        if (not isinstance(key, str) or key.lower() not in HEADERS
                or key.lower() in normalized or not isinstance(value, str)
                or not IDENTIFIER.fullmatch(value)):
            raise Refusal("invalid native account capture headers")
        normalized[key.lower()] = value
    return normalized


def build_env(provider, home, *, control=False, existing_default=False,
              capture_headers=None, environ=None):
    """Build native CLI env; only explicit validated capture headers are restored.

    Inference preserves corporate proxy and CA settings. Control calls match the
    existing uncaptured authentication/quota path by bypassing those overrides.
    existing_default uses vendor defaults and never selects a native profile home.
    """
    if provider not in ("claude", "codex"):
        raise Refusal("unsupported native account provider")
    if not isinstance(control, bool) or not isinstance(existing_default, bool):
        raise Refusal("native environment selectors must be booleans")
    source = os.environ if environ is None else environ
    env = dict(_entries(source))
    for key in list(env):
        if _override(key) or (control and key.upper() in CONTROL_BYPASS):
            env.pop(key)
    if not existing_default:
        try:
            native_home = os.fspath(home)
        except TypeError:
            raise Refusal("an absolute native account home is required")
        if (not isinstance(native_home, str) or not native_home
                or "\0" in native_home or not os.path.isabs(native_home)):
            raise Refusal("an absolute native account home is required")
        env["CLAUDE_CONFIG_DIR" if provider == "claude" else "CODEX_HOME"] = native_home
    if control and provider == "claude":
        env["CLAUDE_NATIVE_DIRECT"] = "1"
    headers = _capture_headers(capture_headers)
    if headers:
        if provider == "claude":
            env["ANTHROPIC_CUSTOM_HEADERS"] = "\n".join(
                key + ": " + headers[key] for key in HEADERS if key in headers)
        else:
            for key in HEADERS:
                if key in headers:
                    env["SB_NATIVE_ACCOUNT_" + key.rsplit("-", 1)[1].upper()] = headers[key]
    return env
