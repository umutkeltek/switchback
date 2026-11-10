"""Native account references and revision-bound launch profiles; never credentials."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
from native_account_process import Refusal

LIMIT = 1024 * 1024

def call(sb, command):
    result = subprocess.run([str(sb), "profile", *command, "--json"],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if len(result.stdout) > LIMIT:
        raise Refusal("account authority response exceeds limit")
    try:
        value = json.loads(result.stdout)
    except (ValueError, UnicodeDecodeError):
        raise Refusal("account authority command failed; inspect sb profile " + command[0])
    if result.returncode:
        raise Refusal("account authority refused " + command[0])
    return value

def binding_id(identity):
    if identity.get("source") and identity.get("value"):
        return identity["source"] + ":" + identity["value"]
    return identity.get("account_uuid") or identity.get("account_id")

def authority_exists(runtime):
    return (Path(runtime) / "config/launch-profiles.json").is_file()

def publish(sb, runtime, provider, name, data, updates=None):
    """Publish only sanitized metadata through the existing authority writer."""
    if not authority_exists(runtime):
        return {"state": "legacy_slot", "registered": False}
    report = call(sb, ["accounts", "list"])
    prior = report["accounts"].get(name)
    home = data.get("native_home", {"kind": "managed", "label": name})
    spec = {"provider": provider, "label": data.get("label") or name,
            "enabled": data.get("enabled", True), "native_home": home,
            "declared": data.get("declared", {})}
    identity = data.get("identity")
    if identity and data.get("identity_observed_at_ms"):
        import time
        spec["observed"] = {
            "account_id": binding_id(identity),
            "observed_at_ms": data["identity_observed_at_ms"]}
        org = identity.get("organization_uuid") or identity.get("organization_id")
        if org:
            spec["observed"]["organization_id"] = org
        plan = data.get("observed_plan")
        if plan:
            spec["observed"]["plan"] = plan
    if prior:
        if prior["provider"] != provider or prior["native_home"] != home:
            raise Refusal("account authority binding differs from native slot")
        # Metadata can be edited through the authority; status refresh must preserve it.
        for key in ("enabled", "label", "declared", "provider_account_ref"):
            if key in prior:
                spec[key] = prior[key]
        if "observed" in prior and "observed" not in spec:
            spec["observed"] = prior["observed"]
        if "observed" in spec and "observed" in prior:
            meaning = lambda observed: {k: v for k, v in observed.items() if k != "observed_at_ms"}
            if meaning(spec["observed"]) == meaning(prior["observed"]):
                spec["observed"] = prior["observed"]
    for key, value in (updates or {}).items():
        if key not in ("label", "declared", "enabled"):
            raise Refusal("unsupported account metadata update")
        spec[key] = {**spec.get(key, {}), **value} if key == "declared" else value
    if spec == prior:
        return {"state": "registered", "registered": True, "account": spec}
    call(sb, ["accounts", "update" if prior else "enroll", name,
              "--spec-json", json.dumps(spec), "--expected-revision", report["authority_revision"]])
    return {"state": "registered", "registered": True, "account": spec}

def metadata_updates(args):
    if args.action in ("enable", "disable"):
        return {"enabled": args.action == "enable"}
    if args.action != "setup":
        return {}
    result = {}
    if args.label:
        result["label"] = args.label
    declared = {key: value for key, value in (("plan", args.plan), ("organization_label", args.organization)) if value}
    if declared:
        result["declared"] = declared
    return result

def resolve_legacy(sb, args, runtime):
    if args.profile or args.pool_name or not args.pool or not authority_exists(runtime):
        return
    report = call(sb, ["accounts", "list"])
    names = args.pool.split(",")
    records = report["accounts"]
    if any(name not in records or records[name]["provider"] != args.provider for name in names):
        raise Refusal("pool slot is not enrolled; run sb account setup for each member")
    args.account_specs = {name: records[name] for name in names}
    args.authority_revision = report["authority_revision"]

def resolve(sb, args):
    if not args.profile:
        return
    if args.pool or args.pool_name:
        raise Refusal("--profile cannot be combined with a pool override")
    report = call(sb, ["show", args.profile])
    profile = report.get("profile", {})
    if profile.get("execution_backend") != "native_account_pool":
        raise Refusal("profile is not a native account pool")
    if profile.get("capture_policy", {}).get("mode") != "segmented_full_wire":
        raise Refusal("native account profiles currently require segmented_full_wire capture")
    policy = profile["account_policy"]
    if getattr(args, "expected_pool_name", None) and args.expected_pool_name != policy["pool"]:
        raise Refusal("profile pool differs from expected pool")
    if args.provider and args.provider != policy["provider"]:
        raise Refusal("profile provider differs from --provider")
    args.provider = policy["provider"]
    args.pool = ",".join(policy["members"])
    args.account_specs = {row["id"]: row for row in policy["accounts"]}
    args.continuation = policy["continuation"]
    args.unknown_quota = policy["unknown_quota"]
    args.authority_revision = report["authority_revision"]
    args.profile_revision = report["revision"]
    args.require_capture = args.require_capture or profile["capture_policy"]["mode"] == "segmented_full_wire"
    args.profile_settings = profile.get("settings", {})

def resolve_pool(sb, args):
    if not args.pool_name:
        return
    if args.pool:
        raise Refusal("--pool-name cannot be combined with --pool")
    report = call(sb, ["pools", "show", args.pool_name])
    pool = report.get("pool") or report.get("account_pool")
    if pool is None:
        pool = report.get("account_pools", {}).get(args.pool_name)
    if not isinstance(pool, dict):
        raise Refusal("named pool response is malformed")
    accounts = call(sb, ["accounts", "list"])
    if report["authority_revision"] != accounts["authority_revision"]:
        raise Refusal("account authority changed during selection; retry")
    if args.provider and args.provider != pool["provider"]:
        raise Refusal("pool provider differs from --provider")
    args.provider = pool["provider"]
    args.pool = ",".join(pool["members"])
    args.account_specs = {name: accounts["accounts"][name] for name in pool["members"]}
    args.continuation, args.unknown_quota = pool["continuation"], pool["unknown_quota"]
    args.authority_revision = report["authority_revision"]

def check_revision(sb, args):
    expected = getattr(args, "authority_revision", None)
    if expected and call(sb, ["accounts", "list"])["authority_revision"] != expected:
        raise Refusal("account authority changed after selection; retry with the current profile")

def account_home(runtime, provider, name, data):
    spec = data.get("native_home", {"kind": "managed", "label": name})
    if spec == {"kind": "existing_default"}:
        return Path.home() / (".claude" if provider == "claude" else ".codex")
    if spec.get("kind") != "managed" or not isinstance(spec.get("label"), str):
        raise Refusal("invalid native home binding")
    import re
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", spec["label"]) or spec["label"].lower() == "default":
        raise Refusal("invalid managed native home")
    return Path(runtime) / "config" / provider / spec["label"]

def default_home(provider, home):
    return Path(home) == Path.home() / (".claude" if provider == "claude" else ".codex")

def check_account(args, name, data):
    spec = getattr(args, "account_specs", {}).get(name)
    if not spec:
        return
    if not spec["enabled"]:
        raise Refusal("account is disabled in authority")
    if spec["native_home"] != data.get("native_home", {"kind": "managed", "label": name}):
        raise Refusal("native slot home differs from authority")
    observed = spec.get("observed")
    binding = data.get("identity") or {}
    if observed and (observed["account_id"] != binding_id(binding)
                     or observed.get("organization_id") != binding.get("organization_uuid", binding.get("organization_id"))):
        raise Refusal("native slot identity differs from authority")

def sync_settings(sb, args, name, data):
    if not args.profile or not args.profile_settings.get("policy_ref"):
        return None
    def plan_current():
        plan = call(sb, ["settings", "plan", args.profile, "--account", name])
        if (plan.get("profile_revision") != args.profile_revision
                or plan.get("authority_revision") != args.authority_revision):
            raise Refusal("shared settings authority changed after selection")
        if "baseline_changed" not in plan or "pending_recovery" not in plan:
            raise Refusal("shared settings owner requires upgrade")
        return plan
    plan = plan_current()
    existing_default = data.get("native_home", {}).get("kind") == "existing_default"
    if plan["pending_recovery"]:
        if existing_default:
            raise Refusal("default settings recovery requires explicit owner command")
        recovered = call(sb, ["settings", "recover", args.profile, "--account", name])
        if recovered.get("conflicts"):
            raise Refusal("shared settings recovery preserved conflicting edits")
        plan = plan_current()
        if plan["pending_recovery"]:
            raise Refusal("shared settings recovery is still pending")
    if plan.get("conflicts"):
        raise Refusal("shared settings conflict; inspect sb profile settings status")
    changed = plan.get("changed") or plan["baseline_changed"]
    if existing_default and changed:
        raise Refusal("default settings need explicit sb profile settings apply --allow-existing-default")
    if changed:
        result = call(sb, ["settings", "apply", args.profile, "--account", name,
                          "--expected-revision", args.profile_revision,
                          "--expected-settings-revision", plan["settings_revision"]])
        result = result.get("plan", result)
        if result.get("conflicts"):
            raise Refusal("shared settings changed concurrently")
        return result.get("effective_revision", "").removeprefix("sha256:") or None
    return plan.get("effective_revision", "").removeprefix("sha256:") or None


def validate_expectations(args):
    import re
    for actual, expected, label in (
            (getattr(args, "profile_revision", None), args.expected_profile_revision, "profile"),
            (getattr(args, "authority_revision", None), args.expected_authority_revision, "authority")):
        if expected and (not re.fullmatch(r"sha256:[0-9a-f]{64}", expected) or actual != expected):
            raise Refusal(label + " revision differs from the admitted launch")
    if bool(args.expected_native_executable) != bool(args.expected_native_sha256):
        raise Refusal("native executable and SHA256 must be supplied together")
    if args.expected_native_executable and (args.provider != "codex" or not args.profile or not args.require_capture):
        raise Refusal("native executable attestation requires a capture-required Codex profile")
    args.account_settings = None
    if args.expected_account_settings_json:
        try:
            expected = json.loads(args.expected_account_settings_json)
        except ValueError:
            raise Refusal("invalid account settings attestations")
        if (not args.profile or not isinstance(expected, dict) or set(expected) != set(args.pool.split(","))
                or any(not isinstance(v, str) or (v != "missing" and not re.fullmatch(r"sha256:[0-9a-f]{64}", v))
                       for v in expected.values())):
            raise Refusal("settings attestations must cover the exact profile pool")
        args.account_settings = expected

def check_settings(args, name, home):
    expected = getattr(args, "account_settings", None)
    if expected is None:
        return None
    import stat
    path = Path(home) / ("settings.json" if args.provider == "claude" else "config.toml")
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        actual = "missing"
    else:
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022 or info.st_size > LIMIT:
                raise Refusal("unsafe native account settings")
            actual = "sha256:" + hashlib.sha256(stream.read(LIMIT + 1)).hexdigest()
    if expected[name] != actual:
        raise Refusal("native account settings changed after admission")
    return actual

def verify_native(args, env):
    if not args.expected_native_executable:
        return None
    import re
    import stat
    requested = Path(args.expected_native_executable)
    if not requested.is_absolute() or not re.fullmatch(r"[0-9a-f]{64}", args.expected_native_sha256 or ""):
        raise Refusal("native executable attestation is invalid")
    executable = requested.resolve(strict=True)
    fd = os.open(executable, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022
                or not os.access(executable, os.X_OK) or info.st_size > 512 * 1024 * 1024):
            raise Refusal("native executable ownership is invalid")
        magic = stream.read(4)
        if magic not in (b"\x7fELF", b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf",
                         b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xce", b"\xca\xfe\xba\xbe",
                         b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"):
            raise Refusal("native executable attestation refuses script wrappers")
        digest = hashlib.sha256(magic)
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
        current = os.stat(executable, follow_symlinks=False)
        if (current.st_dev, current.st_ino, current.st_mtime_ns, current.st_size) != (info.st_dev, info.st_ino, info.st_mtime_ns, info.st_size):
            raise Refusal("native executable changed while verifying")
    if digest.hexdigest() != args.expected_native_sha256:
        raise Refusal("native executable digest differs from admission")
    env["SB_NATIVE_CODEX"] = str(executable)
    return {"path": str(executable), "sha256": digest.hexdigest()}

def validate_claude_settings(home, cwd, read_bytes, capture_required=False):
    """Refuse settings that can silently change native account or capture routing."""
    from native_account_env import validate_settings_env
    candidates = [Path(home) / "settings.json"]
    current = Path(cwd).resolve()
    for directory in (current, *current.parents):
        settings = directory / ".claude/settings.json"
        candidates.extend([settings, settings.with_suffix(".local.json")])
        if (directory / ".git").exists():
            break
    for path in dict.fromkeys(candidates):
        if not path.exists() and not path.is_symlink():
            continue
        try:
            settings = json.loads(read_bytes(path))
        except (ValueError, UnicodeDecodeError):
            raise Refusal("invalid native Claude settings: " + str(path))
        if not isinstance(settings, dict) or any(key in settings for key in
                ("apiKeyHelper", "awsAuthRefresh", "awsCredentialExport", "vertexAuthRefresh")):
            raise Refusal("native Claude settings contain provider overrides: " + str(path))
        environment = settings.get("env", {})
        validate_settings_env(environment)
        if capture_required and any(key.upper() in (
                "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "NODE_EXTRA_CA_CERTS",
                "NODE_TLS_REJECT_UNAUTHORIZED", "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE") or key.upper().startswith("SB_MODE_D_") for key in environment):
            raise Refusal("native settings override required capture routing: " + str(path))


def claude_binding(account, organization):
    if all(isinstance(v, str) and 0 < len(v) <= 256 and not any(ord(c) < 32 for c in v)
           for v in (account, organization)):
        return {"account_uuid": account, "organization_uuid": organization}
    return None


def claude_profile_observation(runtime, name, fs, home=None):
    """Read current vendor identity separately from the saved pool binding; never read tokens."""
    from native_account_usage import claude_windows, summary
    import time
    user_home = Path.home()
    home = Path(home) if home is not None else (
        user_home / ".claude" if name == "default" else Path(runtime) / "config/claude" / name)
    row = {"slot": name, "native_home": str(home),
           "profile_kind": "native_default" if default_home("claude", home) else "managed",
           "email": None, "organization": None, "identity": None, "bound_identity": None,
           "identity_state": "unknown", "authentication": "not_checked",
           "usage": "usage unknown", "same_identity_profiles": []}
    def text(value):
        return "".join(c for c in value if c.isprintable())[:160] if isinstance(value, str) else None
    try:
        meta_path = Path(runtime) / "config/native-account-slots/claude" / name / "slot.json"
        if meta_path.exists() or meta_path.is_symlink():
            fs.safe_dir(meta_path.parent)
            meta = fs.read_json(meta_path)
            if not isinstance(meta, dict) or meta.get("provider") != "claude" or meta.get("slot") != name:
                raise Refusal("invalid profile binding")
            expected = meta.get("identity")
            if isinstance(expected, dict):
                row["bound_identity"] = claude_binding(expected.get("account_uuid"), expected.get("organization_uuid"))
                if row["bound_identity"] is None:
                    raise Refusal("invalid identity binding")
        path = user_home / ".claude.json" if default_home("claude", home) else home / ".claude.json"
        fs.safe_dir(path.parent)
        native = fs.read_json(path)
        identity = native.get("oauthAccount") if isinstance(native, dict) else None
        if not isinstance(identity, dict):
            row["identity_state"] = "not_observed"
            return row
        row["email"] = text(identity.get("emailAddress"))
        row["organization"] = text(identity.get("organizationName"))
        row["identity"] = claude_binding(identity.get("accountUuid"), identity.get("organizationUuid"))
        if row["bound_identity"]:
            row["identity_state"] = ("unknown" if not row["identity"] else
                "matched" if row["identity"] == row["bound_identity"] else "changed")
        else:
            row["identity_state"] = "unbound"
        row["usage"] = summary(claude_windows(native, identity.get("accountUuid"), time.time()))
    except (OSError, ValueError, Refusal):
        row.update(identity_state="unavailable", email=None, organization=None, identity=None)
    return row


def claude_profile_inventory(runtime, fs, preferred="default", slot=None):
    """Profiles remain selectable even when multiple native homes contain the same identity."""
    root = Path(runtime) / "config/claude"
    names = {"default"}
    if root.exists():
        fs.safe_dir(root)
        names.update(p.name for p in root.iterdir()
                     if fs.valid_name(p.name) and p.name != "_providers" and p.is_dir() and not p.is_symlink())
    if slot is not None:
        if not fs.valid_name(slot):
            raise Refusal("invalid Claude profile")
        names.add(slot)
    rows = [claude_profile_observation(runtime, name, fs)
            for name in sorted(names, key=lambda name: (name != preferred, name == "default", name))]
    for row in rows:
        row["preferred"] = row["slot"] == preferred
        row["same_identity_profiles"] = [other["slot"] for other in rows
            if row["identity"] and other["slot"] != row["slot"] and other["identity"] == row["identity"]]
    if slot is not None:
        rows = [row for row in rows if row["slot"] == slot]
    return {"schema": "switchback/claude-profile-observations@1", "accounts": rows,
            "purpose": "native_identity_observation_not_account_rebinding"}


def render_claude_profiles(report, view="text"):
    for row in report["accounts"]:
        if view == "choices" and not row["email"]:
            continue
        labels = [row["slot"], row["email"] or "login not observed"]
        if row["organization"]:
            labels.append(row["organization"])
        if row["profile_kind"] == "native_default":
            labels.append("Claude original profile")
        if row.get("preferred"):
            labels.append("preferred")
        if row["identity_state"] == "changed":
            labels.append("login changed: differs from saved pool binding")
        elif row["identity_state"] in ("unknown", "unavailable"):
            labels.append("identity " + row["identity_state"])
        if row["same_identity_profiles"]:
            labels.append("same login as " + ", ".join(row["same_identity_profiles"]))
        labels.append(row["usage"])
        line = "  ·  ".join(labels)
        print(row["slot"] + "\t" + line if view == "choices" else line)
        if view == "text":
            print("  profile: " + row["native_home"])
