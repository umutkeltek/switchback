#!/usr/bin/env python3
"""Synthetic native environment boundary tests; no credentials are accessed."""
import os
from pathlib import Path
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import native_account_env as native


class NativeEnvironmentTests(unittest.TestCase):
    def test_hostile_inherited_environment_is_removed(self):
        source = {key: "synthetic-hostile-value" for key in native.OVERRIDES}
        source.update({
            "claude_code_use_future_runtime": "1",
            "sb_account_capture_headers": "forged",
            "anthropic_api_key": "synthetic-hostile-value",
            "claude_securestorage_config_dir": "/synthetic/shared",
            "ANTHROPIC_CUSTOM_HEADERS": "Authorization: Bearer synthetic\nx-switchback-account-slot: forged",
            "CLAUDE_CODE_OAUTH_REFRESH_TOKEN": "synthetic-refresh",
            "CLAUDE_CODE_OAUTH_SCOPES": "synthetic-scopes",
            "CLAUDE_CODE_ENTRYPOINT": "nested",
            "CLAUDECODE": "nested",
            "PATH": "/usr/bin:/bin",
        })
        original = source.copy()
        result = native.build_env("claude", "/synthetic/isolated", environ=source)
        self.assertEqual(result, {"PATH": "/usr/bin:/bin", "CLAUDE_CONFIG_DIR": "/synthetic/isolated"})
        self.assertEqual(source, original)

    def test_inference_preserves_corporate_proxy_and_preferences(self):
        source = {
            "HTTPS_PROXY": "https://proxy.example:443",
            "https_proxy": "https://proxy.example:443",
            "HTTP_PROXY": "http://proxy.example:8080",
            "ALL_PROXY": "socks5://proxy.example:1080",
            "NO_PROXY": ".example",
            "NODE_EXTRA_CA_CERTS": "/synthetic/corporate-ca.pem",
            "SSL_CERT_FILE": "/synthetic/corporate-ca.pem",
            "ANTHROPIC_MODEL": "synthetic-model",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "synthetic-model",
            "OPENAI_MODEL": "synthetic-model",
            "CLAUDE_CODE_EFFORT_LEVEL": "high",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS": "1024",
            "SB_LAUNCH_CAPTURE_POLICY": "metadata_only",
        }
        result = native.build_env("claude", Path("/synthetic/native"), environ=source)
        self.assertEqual({k: result[k] for k in source}, source)
        self.assertNotIn("CLAUDE_NATIVE_DIRECT", result)

    def test_control_removes_proxy_and_nonessential_traffic_switch(self):
        source = {
            key: "synthetic-value" for key in native.CONTROL_BYPASS
        }
        source.update({
            "https_proxy": "https://proxy.example:443",
            "node_extra_ca_certs": "/synthetic/ca.pem",
            "claude_code_disable_nonessential_traffic": "1",
            "CLAUDE_NATIVE_DIRECT": "0",
            "NO_PROXY": ".example",
            "SSL_CERT_FILE": "/synthetic/corporate-ca.pem",
        })
        result = native.build_env("claude", "/synthetic/native", control=True, environ=source)
        self.assertEqual(result, {
            "CLAUDE_CONFIG_DIR": "/synthetic/native",
            "CLAUDE_NATIVE_DIRECT": "1",
            "NO_PROXY": ".example",
            "SSL_CERT_FILE": "/synthetic/corporate-ca.pem",
        })

    def test_existing_default_claude_leaves_native_config_paths_unset(self):
        source = {
            "CLAUDE_CONFIG_DIR": "/synthetic/profile",
            "claude_config_dir": "/synthetic/profile",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR": "/synthetic/secure-storage",
            "CODEX_HOME": "/synthetic/codex",
            "HOME": "/synthetic/user",
        }
        for control in (False, True):
            result = native.build_env("claude", None, existing_default=True,
                                      control=control, environ=source)
            self.assertEqual(result["HOME"], "/synthetic/user")
            for key in ("CLAUDE_CONFIG_DIR", "claude_config_dir", "CLAUDE_SECURESTORAGE_CONFIG_DIR", "CODEX_HOME"):
                self.assertNotIn(key, result)
            self.assertEqual(result.get("CLAUDE_NATIVE_DIRECT"), "1" if control else None)

    def test_codex_home_is_native_and_default_is_unset(self):
        source = {"CODEX_HOME": "/synthetic/inherited", "CLAUDE_CONFIG_DIR": "/synthetic/inherited",
                  "CLAUDE_NATIVE_DIRECT": "1", "OPENAI_API_KEY": "synthetic-key"}
        result = native.build_env("codex", "/synthetic/isolated", environ=source)
        self.assertEqual(result, {"CODEX_HOME": "/synthetic/isolated"})
        self.assertEqual(native.build_env("codex", None, existing_default=True, environ=source), {})
        self.assertEqual(native.build_env("codex", None, existing_default=True,
                                         control=True, environ=source), {})

    def test_only_explicit_validated_capture_headers_are_restored(self):
        headers = {
            "X-Switchback-Account-Session": "synthetic-session",
            "x-switchback-account-attempt": "attempt_0123456789",
            "x-switchback-account-slot": "test-slot",
        }
        result = native.build_env("claude", "/synthetic/native", environ={
            "ANTHROPIC_CUSTOM_HEADERS": "Authorization: Bearer synthetic-secret",
        }, capture_headers=headers)
        self.assertEqual(result["ANTHROPIC_CUSTOM_HEADERS"],
                         "x-switchback-account-attempt: attempt_0123456789\n"
                         "x-switchback-account-session: synthetic-session\n"
                         "x-switchback-account-slot: test-slot")
        self.assertNotIn("Authorization", result["ANTHROPIC_CUSTOM_HEADERS"])
        self.assertEqual(headers["X-Switchback-Account-Session"], "synthetic-session")
        without = native.build_env("claude", "/synthetic/native", environ={
            "ANTHROPIC_CUSTOM_HEADERS": "x-switchback-account-slot: forged",
        }, capture_headers={})
        self.assertNotIn("ANTHROPIC_CUSTOM_HEADERS", without)

    def test_codex_uses_dedicated_capture_environment(self):
        source = {
            "SB_NATIVE_ACCOUNT_ATTEMPT": "forged-attempt",
            "sb_native_account_session": "forged-session",
            "SB_NATIVE_ACCOUNT_SLOT": "forged-slot",
            "SB_ACCOUNT_CAPTURE_HEADERS": "forged-header",
            "ANTHROPIC_CUSTOM_HEADERS": "Authorization: synthetic",
        }
        headers = {
            "x-switchback-account-attempt": "native-attempt",
            "x-switchback-account-session": "native-session",
            "x-switchback-account-slot": "native-slot",
        }
        result = native.build_env("codex", "/synthetic/native", environ=source,
                                  capture_headers=headers)
        self.assertEqual(result, {
            "CODEX_HOME": "/synthetic/native",
            "SB_NATIVE_ACCOUNT_ATTEMPT": "native-attempt",
            "SB_NATIVE_ACCOUNT_SESSION": "native-session",
            "SB_NATIVE_ACCOUNT_SLOT": "native-slot",
        })
        self.assertEqual(native.build_env("codex", "/synthetic/native", environ=source),
                         {"CODEX_HOME": "/synthetic/native"})

    def test_capture_headers_refuse_unknown_or_injected_values(self):
        bad = (
            "x-switchback-account-slot: test",
            {"Authorization": "synthetic"},
            {"x-switchback-account-slot": "test\nAuthorization: synthetic"},
            {"x-switchback-account-slot": "test\rsynthetic"},
            {"x-switchback-account-slot": "test\0synthetic"},
            {"x-switchback-account-slot": "x" * 129},
            {"x-switchback-account-slot": ""},
            {"x-switchback-account-slot": "space in value"},
            {"x-switchback-account-slot": "https://capture.example"},
            {"x-switchback-account-slot": 7},
            {"x-switchback-account-slot": "a", "X-Switchback-Account-Slot": "b"},
        )
        for headers in bad:
            with self.subTest(headers=headers), self.assertRaises(native.Refusal):
                native.build_env("claude", "/synthetic/native", environ={}, capture_headers=headers)

    def test_settings_env_refuses_native_override_selectors(self):
        for key in native.OVERRIDES | {"CLAUDE_CODE_USE_FUTURE_PROVIDER", "SB_ACCOUNT_CAPTURE_HEADER",
                                        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"}:
            with self.subTest(key=key), self.assertRaises(native.Refusal):
                native.validate_settings_env({key.lower(): "synthetic"})
        native.validate_settings_env({
            "ANTHROPIC_MODEL": "synthetic-model",
            "CLAUDE_CODE_EFFORT_LEVEL": "high",
            "HTTPS_PROXY": "https://proxy.example:443",
            "LANG": "en_US.UTF-8",
        })

    def test_settings_env_requires_string_mapping(self):
        for value in (None, [], {"X": None}, {"X": 1}, {1: "value"},
                      {"": "value"}, {"X=Y": "value"}, {"X": "value\0"}):
            with self.subTest(value=value), self.assertRaises(native.Refusal):
                native.validate_settings_env(value)

    def test_environment_default_is_copied(self):
        with mock.patch.dict(os.environ, {"ANTHROPIC_API_KEY": "synthetic-key", "LANG": "synthetic"}, clear=True):
            result = native.build_env("claude", "/synthetic/native")
            self.assertEqual(result, {"LANG": "synthetic", "CLAUDE_CONFIG_DIR": "/synthetic/native"})
            self.assertEqual(os.environ["ANTHROPIC_API_KEY"], "synthetic-key")

    def test_invalid_provider_home_and_flags(self):
        for provider, home, flags in (
                ("unknown", "/synthetic/native", {}),
                ("claude", "relative", {}),
                ("claude", None, {}),
                ("codex", "/synthetic/native\0", {}),
                ("codex", "/synthetic/native", {"control": "yes"}),
                ("codex", "/synthetic/native", {"existing_default": 1})):
            with self.subTest(provider=provider, home=home), self.assertRaises(native.Refusal):
                native.build_env(provider, home, environ={}, **flags)


if __name__ == "__main__":
    unittest.main()
