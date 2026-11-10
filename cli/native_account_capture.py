"""Exact native attempt capture verification. No body text leaves the verifier."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
from native_account_process import Refusal

def probe(route, provider, state, listener_id=None, binding=None):
    port = route.get("proxy_port" if provider == "claude" else "tap_port")
    if type(port) is not int or not 1024 <= port <= 65535:
        raise Refusal("invalid native capture listener")
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
    try:
        client.request("GET", "/.well-known/switchback/native-account-capture")
        response = client.getresponse()
        body = response.read(65537)
        if response.status != 200 or len(body) > 65536:
            raise Refusal("recorder does not attest account capture support")
        capability = json.loads(body)
        if not isinstance(capability, dict):
            raise Refusal("recorder capability response is invalid")
        if (capability.get("schema") != "switchback/native-account-capture@1"
                or (listener_id is not None and capability.get("listener_id") != listener_id)
                or capability.get("capture_available") is not True
                or capability.get("observed_capture_mode") != "segmented_full_wire"
                or capability.get("verifier_schema") != "switchback/body-attempt-verification@1"
                or capability.get("listener_kind") != ("forward_proxy" if provider == "claude" else "tap")
                or Path(capability.get("body_state_dir", "")) != state
                or not {"x-switchback-account-attempt", "x-switchback-account-session", "x-switchback-account-slot"}.issubset(set(capability.get("correlation_headers", [])))):
            raise Refusal("recorder capability differs from required native capture")
        if binding and any(capability.get(key) != value for key, value in binding.items()):
            raise Refusal("live recorder enrollment differs from selected configuration")
        return capability
    except (OSError, ValueError, TypeError, http.client.HTTPException) as error:
        raise Refusal("recorder capability unavailable or malformed") from error
    finally:
        client.close()

def prepare(runtime, provider, fs, required=False):
    path = Path(runtime) / "config" / ("native-" + provider + "-capture.json")
    try:
        route = fs.read_json(path)
        if not route or route.get("schema") != "switchback/native-" + provider + "-capture@1":
            raise Refusal("native capture route unavailable")
        config = Path(route["capture_config"])
        if not config.is_absolute() or hashlib.sha256(fs.read_owned_bytes(config)).hexdigest() != route["capture_config_sha256"]:
            raise Refusal("native capture route configuration changed")
        server = json.loads(fs.read_owned_bytes(config))["server"]
        port = route.get("proxy_port" if provider == "claude" else "tap_port")
        field = "forward_proxies" if provider == "claude" else "taps"
        listeners = [item for item in server.get(field, []) if item.get("bind") == "127.0.0.1:" + str(port)]
        if len(listeners) != 1 or listeners[0].get("capture_bodies") is not True:
            raise Refusal("capture route is not bound to one full-body listener")
        listener = listeners[0]
        listener_id = listener.get("id")
        if not isinstance(listener_id, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", listener_id):
            raise Refusal("invalid capture listener identity")
        normalize = lambda host: host.strip().rstrip(".").lower()
        if provider == "claude":
            if listener.get("capture_harness") != "claude" or "api.anthropic.com" not in [normalize(h) for h in listener.get("intercept_hosts", [])]:
                raise Refusal("native Claude inference host is not enrolled for interception")
        elif listener.get("headers", {}).get("x-switchback-harness") != "codex" or not isinstance(listener.get("upstream"), str):
            raise Refusal("native Codex capture upstream is not enrolled")
        if provider == "claude":
            upstream = "https://api.anthropic.com"
            for host, candidate in listener.get("upstream_overrides", {}).items():
                if normalize(host) == "api.anthropic.com":
                    upstream = candidate
            for candidate in listener.get("upstream_routes", []):
                prefixes = candidate.get("path_prefixes", [])
                if normalize(candidate["host"]) == "api.anthropic.com" and (not prefixes or any("/v1/messages".startswith(prefix) for prefix in prefixes)):
                    upstream = candidate["upstream"]
                    break
            binding = {"capture_harness": "claude", "native_claude_intercepted": True,
                       "selected_messages_upstream_sha256": "sha256:" + hashlib.sha256(upstream.rstrip("/").encode()).hexdigest()}
        else:
            binding = {"capture_harness": "codex",
                       "upstream_sha256": "sha256:" + hashlib.sha256(listener["upstream"].rstrip("/").encode()).hexdigest()}
        label = route["service_label"]
        if not re.fullmatch(r"[a-z0-9.-]+", label):
            raise Refusal("invalid capture service reference")
        # The service definition is the existing owner of its storage root.
        plist = Path.home() / "Library/LaunchAgents" / (label + ".plist")
        service = plistlib.loads(fs.read_owned_bytes(plist))
        argv = service["ProgramArguments"]
        if service.get("Label") != label or not isinstance(argv, list) or not all(isinstance(a, str) for a in argv):
            raise Refusal("capture service ownership is unproven")
        if argv.count("--config") != 1 or Path(argv[argv.index("--config") + 1]) != config:
            raise Refusal("capture service differs from enrolled route")
        state_arg = argv[argv.index("--state-dir") + 1] if argv.count("--state-dir") == 1 else service.get("EnvironmentVariables", {}).get("SWITCHBACK_BODY_STATE_DIR")
        if not isinstance(state_arg, str) or not Path(state_arg).is_absolute():
            raise Refusal("capture service storage root unavailable")
        state = fs.safe_dir(Path(state_arg))
        capability = probe(route, provider, state, listener_id, binding)
        entrypoint = Path(runtime) / "bin/entrypoints" / ("claude" if provider == "claude" else "codex-switchback-tap")
        marker = b"switchback-owned: native-claude-mode-d-entrypoint@1" if provider == "claude" else b"switchback-owned: native-codex-capture-entrypoint@2"
        entrypoint_body = fs.read_owned_bytes(entrypoint)
        if marker not in entrypoint_body:
            raise Refusal("owned native capture entrypoint requires upgrade")
        binary = Path(runtime) / "bin/switchback-bin"
        # Runtime installs may use an owned symlink to a versioned engine.
        binary = binary.resolve(strict=True)
        fs.read_owned_bytes(binary) if binary.stat().st_size < 1024 * 1024 else None
        if not binary.is_file() or binary.stat().st_uid != os.getuid() or binary.stat().st_mode & 0o022:
            raise Refusal("capture verifier executable is not owned")
        result = {"state": "ready", "state_dir": str(state), "binary": str(binary),
                  "route_sha256": hashlib.sha256(fs.read_owned_bytes(path)).hexdigest(),
                  "config_sha256": route["capture_config_sha256"], "route_path": str(path),
                  "provider": provider, "route": route, "capability": capability, "listener_id": listener_id, "binding": binding,
                  "entrypoint": str(entrypoint), "entrypoint_sha256": hashlib.sha256(entrypoint_body).hexdigest()}
        if required:
            proof = verify(result, "preflight", wait=0)
            if proof.get("state") == "unavailable":
                raise Refusal("installed capture verifier unavailable")
        return result
    except (OSError, ValueError, KeyError, IndexError, TypeError, http.client.HTTPException, Refusal) as error:
        if required:
            raise Refusal("capture preflight failed: " + str(error))
        return {"state": "unavailable", "reason": "capture_route_unproven"}

def verify(context, attempt, wait=5, slot=None, session=None):
    if context.get("state") != "ready":
        return {"state": "unavailable", "complete": False, "reason": context.get("reason", "capture_route_unproven")}
    try:
        process = subprocess.run([context["binary"], "--json", "body", "verify-attempt",
                                  "--state-dir", context["state_dir"], "--attempt", attempt,
                                  "--wait", str(wait)],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=wait + 10)
        if len(process.stdout) > 2 * 1024 * 1024:
            raise ValueError("large response")
        report = json.loads(process.stdout)
        if not isinstance(report, dict):
            raise ValueError("invalid verifier report")
        if process.returncode or report.get("schema") != "switchback/body-attempt-verification@1" or report.get("attempt_id") != attempt:
            raise ValueError("verifier response invalid")
        if (report.get("state_dir") != context["state_dir"]
                or report.get("scope") != "observed_inference_requests"
                or report.get("attempt_completion") != "not_attested"):
            raise ValueError("verifier scope differs from native receipt")
        if report.get("complete") is True:
            requests = report.get("requests")
            if (not isinstance(requests, list) or not requests
                    or any(not isinstance(request, dict) or request.get("complete") is not True
                           or (slot is not None and request.get("account_slot") != slot)
                           or (session is not None and request.get("account_session_id") != session)
                           for request in requests)):
                raise ValueError("verifier account binding differs from native receipt")
        report["state"] = "verified" if report.get("complete") is True else "unverified"
        return report
    except (OSError, ValueError, TypeError, subprocess.TimeoutExpired):
        return {"state": "unavailable", "complete": False, "reason": "verifier_unavailable"}

def recheck(context, fs):
    if context.get("state") == "ready":
        if hashlib.sha256(fs.read_owned_bytes(Path(context["route_path"]))).hexdigest() != context["route_sha256"]:
            raise Refusal("native capture route changed before launch")
        if hashlib.sha256(fs.read_owned_bytes(Path(context["entrypoint"]))).hexdigest() != context["entrypoint_sha256"]:
            raise Refusal("native capture entrypoint changed before launch")
        probe(context["route"], context["provider"], Path(context["state_dir"]), context["listener_id"], context["binding"])

def configure_env(context, env):
    if context.get("state") != "ready":
        return env
    env = dict(env)
    if context["provider"] == "codex":
        for key in list(env):
            if key.upper() in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "SWITCHBACK_LEGACY_ENV"):
                env.pop(key)
        env["NO_PROXY"] = env["no_proxy"] = "localhost,127.0.0.1,::1"
    if context["provider"] == "claude":
        for key in list(env):
            if key.upper().startswith("SB_MODE_D_") or key.upper() in (
                    "SB_NATIVE_CLAUDE_PIN_FILE", "SB_NATIVE_CLAUDE_VERSIONS_DIR", "CLAUDE_NATIVE_DIRECT"):
                env.pop(key)
        route = context["route"]
        env.update(SB_MODE_D_HOST="127.0.0.1", SB_MODE_D_PORT=str(route["proxy_port"]),
                   SB_MODE_D_CA_CERT=route["ca_cert"], SB_MODE_D_LAUNCH_LABEL=route["service_label"])
    return env

def finish(receipt, context, persist=None):
    receipt["provider_outcome"] = receipt["disposition"]
    receipt["attempt_terminal"] = True
    receipt["capture"] = {"state": "pending", "complete": False}
    # Persist native completion before an asynchronous recorder check can fail,
    # time out, or lose its supervising process. Recovery must never replay it.
    if persist:
        persist(receipt)
    receipt["capture"] = verify(context, receipt["attempt_id"], slot=receipt.get("slot"), session=receipt.get("account_session_id"))

def capture_exit(receipt, required):
    if required and receipt.get("capture", {}).get("complete") is not True:
        import sys
        print("account: native task completed; capture is unverified; task was not replayed", file=sys.stderr)
        return 74
    return 0
