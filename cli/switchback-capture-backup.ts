#!/usr/bin/env bun

import { chmod, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join, posix } from "node:path";

type BackupPlanItem = {
  segment_file: string;
  segment_path: string;
  manifest_path: string;
  segment_sha256: string;
  manifest_sha256: string;
  segment_bytes: number;
  record_count: number;
  first_observed_at_unix_ms: number;
  last_observed_at_unix_ms: number;
  utc_day: string;
};

type BackupPlan = {
  schema: string;
  next_generation: number;
  segments: BackupPlanItem[];
  total_segment_bytes: number;
};

type ReceiptItem = {
  segment_sha256: string;
  manifest_sha256: string;
  remote_path: string;
  remote_manifest_path: string;
  remote_checksum_verified: true;
};

type LegacyBackupPlanItem = {
  artifact_id: string;
  kind: string;
  file_name: string;
  local_path: string;
  sha256: string;
  bytes: number;
  modified_at_unix_ms: number;
};

type LegacyBackupPlan = {
  schema: string;
  artifacts: LegacyBackupPlanItem[];
  total_artifact_bytes: number;
  blockers: LegacyBackupBlocker[];
};

type LegacyBackupBlocker = {
  code: string;
  artifact_id: string;
  local_path: string;
  remediation: string;
};

type LegacyReceiptItem = {
  artifact_id: string;
  kind: string;
  local_path: string;
  sha256: string;
  bytes: number;
  modified_at_unix_ms: number;
  remote_path: string;
  remote_checksum_verified: true;
};

type ReclaimPlanItem = {
  segment_sha256: string;
  manifest_sha256: string;
  remote_root: string;
  remote_path: string;
  remote_manifest_path: string;
};

type ReclaimPlan = {
  schema: string;
  keep_days: number;
  segments: ReclaimPlanItem[];
  total_segment_bytes: number;
};

type CommandResult = {
  stdout: string;
  stderr: string;
};

const BACKUP_PLAN_SCHEMA = "switchback/capture-backup-plan@1";
const BACKUP_RECEIPT_SCHEMA = "switchback/capture-backup@2";
const LEGACY_BACKUP_PLAN_SCHEMA =
  "switchback/capture-legacy-backup-plan@1";
const LEGACY_BACKUP_RECEIPT_SCHEMA = "switchback/capture-legacy-backup@1";
const TRANSFER_RESULT_SCHEMA = "switchback/capture-backup-transfer@1";
const RECLAIM_PLAN_SCHEMA = "switchback/capture-reclaim-plan@1";
const RECLAIM_PROOF_SCHEMA = "switchback/capture-reclaim-proof@1";

const remoteScript = `
set -euo pipefail
umask 077

# Capture bodies are protected evidence: nothing here may be reachable by other
# users. Permissions are the dataset's to grant, not ours to set — the capture
# pool is acltype=nfsv4 with aclmode=restricted, where chmod is denied outright
# even to the owner, so every attempt to set modes on arrival fails with EPERM
# (which is also why the transfer no longer asks rsync to set them). Verify the
# guarantee the ACL is supposed to provide, and refuse to promote if it ever
# starts handing out world access.
assert_owner_only() {
  target="\${1:?missing target}"
  offenders="$(find "$target" -perm /o+rwx -print | head -n 5)"
  if test -n "$offenders"; then
    printf 'refusing world-accessible capture artifacts under %s:\n%s\n' \
      "$target" "$offenders" >&2
    exit 65
  fi
}

tree_fingerprint() {
  tree="\${1:?missing tree}"
  (
    cd -- "$tree"
    find . -type f -print0 |
      LC_ALL=C sort -z |
      while IFS= read -r -d '' relative; do
        relative="\${relative#./}"
        sha="$(sha256sum -- "$relative" | awk '{print $1}')"
        bytes="$(stat -c '%s' -- "$relative")"
        printf '%s\t%s\t%s\n' "$sha" "$bytes" "$relative"
      done
  ) | sha256sum | awk '{print $1}'
}

action="\${1:?missing action}"
shift
case "$action" in
  ensure)
    root="\${1:?missing root}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    mkdir -p -- "$root/segments" "$root/legacy" "$root/.incoming"
    probe="$root/.incoming/.write-probe-$$"
    : > "$probe"
    rm -f -- "$probe"
    ;;
  prepare)
    root="\${1:?missing root}"
    staging_rel="\${2:?missing staging path}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$staging_rel" in
      .incoming/generation-*) ;;
      *) printf 'refusing unexpected staging path: %s\n' "$staging_rel" >&2; exit 64 ;;
    esac
    mkdir -p -- "$root/$staging_rel"
    ;;
  promote)
    root="\${1:?missing root}"
    staging_rel="\${2:?missing staging path}"
    final_rel="\${3:?missing final path}"
    segment_file="\${4:?missing segment file}"
    expected_segment_sha="\${5:?missing segment checksum}"
    manifest_file="\${6:?missing manifest file}"
    expected_manifest_sha="\${7:?missing manifest checksum}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$staging_rel" in
      .incoming/generation-*) ;;
      *) printf 'refusing unexpected staging path: %s\n' "$staging_rel" >&2; exit 64 ;;
    esac
    case "$final_rel" in
      segments/20??/??/??/[0-9a-f][0-9a-f]*) ;;
      *) printf 'refusing unexpected final path: %s\n' "$final_rel" >&2; exit 64 ;;
    esac
    staging="$root/$staging_rel"
    final="$root/$final_rel"
    assert_owner_only "$staging"
    test -f "$staging/$segment_file"
    test -f "$staging/$manifest_file"
    staging_entry_count="$(find "$staging" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' ')"
    test "$staging_entry_count" = "2"
    actual_segment_sha="$(sha256sum "$staging/$segment_file" | awk '{print $1}')"
    actual_manifest_sha="$(sha256sum "$staging/$manifest_file" | awk '{print $1}')"
    test "$actual_segment_sha" = "$expected_segment_sha"
    test "$actual_manifest_sha" = "$expected_manifest_sha"
    mkdir -p -- "$(dirname "$final")"
    if test -d "$final"; then
      existing_segment_sha="$(sha256sum "$final/$segment_file" | awk '{print $1}')"
      existing_manifest_sha="$(sha256sum "$final/$manifest_file" | awk '{print $1}')"
      test "$existing_segment_sha" = "$expected_segment_sha"
      test "$existing_manifest_sha" = "$expected_manifest_sha"
      rm -rf -- "$staging"
    else
      mv -- "$staging" "$final"
    fi
    ;;
  verify)
    root="\${1:?missing root}"
    segment_rel="\${2:?missing segment path}"
    expected_segment_sha="\${3:?missing segment checksum}"
    manifest_rel="\${4:?missing manifest path}"
    expected_manifest_sha="\${5:?missing manifest checksum}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$segment_rel" in
      segments/20??/??/??/[0-9a-f][0-9a-f]*/*.sbcap) ;;
      *) printf 'refusing unexpected remote segment path: %s\n' "$segment_rel" >&2; exit 64 ;;
    esac
    case "$manifest_rel" in
      segments/20??/??/??/[0-9a-f][0-9a-f]*/*.sbcap.manifest.json) ;;
      *) printf 'refusing unexpected remote manifest path: %s\n' "$manifest_rel" >&2; exit 64 ;;
    esac
    actual_segment_sha="$(sha256sum "$root/$segment_rel" | awk '{print $1}')"
    actual_manifest_sha="$(sha256sum "$root/$manifest_rel" | awk '{print $1}')"
    test "$actual_segment_sha" = "$expected_segment_sha"
    test "$actual_manifest_sha" = "$expected_manifest_sha"
    ;;
  prepare-legacy)
    root="\${1:?missing root}"
    staging_rel="\${2:?missing staging path}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$staging_rel" in
      .incoming/legacy-*) ;;
      *) printf 'refusing unexpected legacy staging path: %s\n' "$staging_rel" >&2; exit 64 ;;
    esac
    mkdir -p -- "$root/$staging_rel"
    ;;
  promote-legacy)
    root="\${1:?missing root}"
    staging_rel="\${2:?missing staging path}"
    final_rel="\${3:?missing final path}"
    file_name="\${4:?missing file name}"
    expected_sha="\${5:?missing checksum}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$staging_rel" in
      .incoming/legacy-*) ;;
      *) printf 'refusing unexpected legacy staging path: %s\n' "$staging_rel" >&2; exit 64 ;;
    esac
    case "$final_rel" in
      legacy/*/[0-9a-f][0-9a-f]*) ;;
      *) printf 'refusing unexpected legacy final path: %s\n' "$final_rel" >&2; exit 64 ;;
    esac
    staging="$root/$staging_rel"
    final="$root/$final_rel"
    assert_owner_only "$staging"
    test -f "$staging/$file_name"
    staging_entry_count="$(find "$staging" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' ')"
    test "$staging_entry_count" = "1"
    actual_sha="$(sha256sum "$staging/$file_name" | awk '{print $1}')"
    test "$actual_sha" = "$expected_sha"
    mkdir -p -- "$(dirname "$final")"
    if test -d "$final"; then
      existing_sha="$(sha256sum "$final/$file_name" | awk '{print $1}')"
      test "$existing_sha" = "$expected_sha"
      rm -rf -- "$staging"
    else
      mv -- "$staging" "$final"
    fi
    ;;
  verify-legacy)
    root="\${1:?missing root}"
    artifact_rel="\${2:?missing artifact path}"
    expected_sha="\${3:?missing checksum}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$artifact_rel" in
      legacy/*/[0-9a-f][0-9a-f]*/*) ;;
      *) printf 'refusing unexpected legacy artifact path: %s\n' "$artifact_rel" >&2; exit 64 ;;
    esac
    actual_sha="$(sha256sum "$root/$artifact_rel" | awk '{print $1}')"
    test "$actual_sha" = "$expected_sha"
    ;;
  promote-legacy-tree)
    root="\${1:?missing root}"
    staging_rel="\${2:?missing staging path}"
    final_rel="\${3:?missing final path}"
    directory_name="\${4:?missing directory name}"
    expected_sha="\${5:?missing checksum}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$staging_rel" in
      .incoming/legacy-*) ;;
      *) printf 'refusing unexpected legacy staging path: %s\n' "$staging_rel" >&2; exit 64 ;;
    esac
    case "$final_rel" in
      legacy/*/[0-9a-f][0-9a-f]*) ;;
      *) printf 'refusing unexpected legacy final path: %s\n' "$final_rel" >&2; exit 64 ;;
    esac
    staging="$root/$staging_rel"
    final="$root/$final_rel"
    assert_owner_only "$staging"
    test -d "$staging/$directory_name"
    staging_entry_count="$(find "$staging" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' ')"
    test "$staging_entry_count" = "1"
    actual_sha="$(tree_fingerprint "$staging/$directory_name")"
    test "$actual_sha" = "$expected_sha"
    mkdir -p -- "$(dirname "$final")"
    if test -d "$final"; then
      existing_sha="$(tree_fingerprint "$final/$directory_name")"
      test "$existing_sha" = "$expected_sha"
      rm -rf -- "$staging"
    else
      mv -- "$staging" "$final"
    fi
    ;;
  verify-legacy-tree)
    root="\${1:?missing root}"
    artifact_rel="\${2:?missing artifact path}"
    expected_sha="\${3:?missing checksum}"
    case "$root" in
      /mnt/*) ;;
      *) printf 'refusing unexpected backup root: %s\n' "$root" >&2; exit 64 ;;
    esac
    case "$artifact_rel" in
      legacy/*/[0-9a-f][0-9a-f]*/*) ;;
      *) printf 'refusing unexpected legacy artifact path: %s\n' "$artifact_rel" >&2; exit 64 ;;
    esac
    test -d "$root/$artifact_rel"
    actual_sha="$(tree_fingerprint "$root/$artifact_rel")"
    test "$actual_sha" = "$expected_sha"
    ;;
  *)
    printf 'unknown remote backup action: %s\n' "$action" >&2
    exit 64
    ;;
esac
`;

function fail(message: string): never {
  throw new Error(message);
}

function assertSafeToken(value: string, label: string): void {
  if (!/^[A-Za-z0-9._@/-]+$/.test(value)) {
    fail(`${label} contains unsupported characters`);
  }
}

function assertSha256(value: string, label: string): void {
  if (!/^[0-9a-f]{64}$/.test(value)) {
    fail(`${label} is not a lowercase SHA-256`);
  }
}

function assertRemoteRoot(value: string): void {
  assertSafeToken(value, "backup remote root");
  if (
    !posix.isAbsolute(value) ||
    !value.startsWith("/mnt/") ||
    posix.normalize(value) !== value ||
    value.endsWith("/")
  ) {
    fail("backup remote root must be a normalized absolute path below /mnt");
  }
}

function assertRemoteRelative(value: string, label: string): void {
  assertSafeToken(value, label);
  if (
    posix.isAbsolute(value) ||
    posix.normalize(value) !== value ||
    value.startsWith("../") ||
    value.includes("/../") ||
    !value.startsWith("segments/")
  ) {
    fail(`${label} must be a normalized path below segments`);
  }
}

function assertLegacyRemoteRelative(value: string, label: string): void {
  assertSafeToken(value, label);
  if (
    posix.isAbsolute(value) ||
    posix.normalize(value) !== value ||
    value.startsWith("../") ||
    value.includes("/../") ||
    !value.startsWith("legacy/")
  ) {
    fail(`${label} must be a normalized path below legacy`);
  }
}

function envFlag(name: string): boolean {
  const value = process.env[name] || "0";
  if (value !== "0" && value !== "1") {
    fail(`${name} must be 0 or 1`);
  }
  return value === "1";
}

function reclaimKeepDays(): number {
  const raw = process.env.SWITCHBACK_BACKUP_KEEP_DAYS || "14";
  if (!/^[0-9]+$/.test(raw)) {
    fail("SWITCHBACK_BACKUP_KEEP_DAYS must be a non-negative integer");
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value)) {
    fail("SWITCHBACK_BACKUP_KEEP_DAYS is outside the safe integer range");
  }
  return value;
}

function parseMode(argv: string[]): "transfer" | "plan" | "verify" {
  if (argv.length === 0) return "transfer";
  if (argv.length === 1 && argv[0] === "--plan") return "plan";
  if (argv.length === 1 && argv[0] === "--verify-only") return "verify";
  fail("usage: switchback-capture-backup.ts [--plan|--verify-only]");
}

async function run(
  command: string[],
  stdin?: string,
): Promise<CommandResult> {
  const child = Bun.spawn(command, {
    stdin: stdin === undefined ? "ignore" : "pipe",
    stdout: "pipe",
    stderr: "pipe",
    env: process.env,
  });
  if (stdin !== undefined) {
    child.stdin.write(stdin);
    child.stdin.end();
  }
  const stdoutPromise = new Response(child.stdout).text();
  const stderrPromise = new Response(child.stderr).text();
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    stdoutPromise,
    stderrPromise,
  ]);
  if (exitCode !== 0) {
    fail(
      `command failed (${exitCode}): ${command.join(" ")}${
        stderr.trim() ? `\n${stderr.trim()}` : ""
      }`,
    );
  }
  return { stdout, stderr };
}

function sshCommand(remote: string): string[] {
  return [
    "ssh",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ConnectTimeout=15",
    remote,
  ];
}

async function remoteAction(
  remote: string,
  action: string,
  args: string[],
): Promise<void> {
  await run(
    [...sshCommand(remote), "bash", "-s", "--", action, ...args],
    remoteScript,
  );
}

async function localSha256(path: string): Promise<string> {
  const result = await run(["shasum", "-a", "256", path]);
  const checksum = result.stdout.trim().split(/\s+/, 1)[0] ?? "";
  assertSha256(checksum, `checksum for ${path}`);
  return checksum;
}

function validatePlan(plan: BackupPlan): void {
  if (plan.schema !== BACKUP_PLAN_SCHEMA) {
    fail(`unsupported backup plan schema: ${plan.schema}`);
  }
  if (!Number.isSafeInteger(plan.next_generation) || plan.next_generation < 1) {
    fail("backup plan generation is invalid");
  }
  if (!Array.isArray(plan.segments)) {
    fail("backup plan segments are invalid");
  }
  const hashes = new Set<string>();
  for (const segment of plan.segments) {
    assertSafeToken(segment.segment_file, "segment file");
    if (basename(segment.segment_path) !== segment.segment_file) {
      fail(`segment path/file mismatch: ${segment.segment_path}`);
    }
    if (basename(segment.manifest_path) !== `${segment.segment_file}.manifest.json`) {
      fail(`manifest path/file mismatch: ${segment.manifest_path}`);
    }
    assertSha256(segment.segment_sha256, "segment checksum");
    assertSha256(segment.manifest_sha256, "manifest checksum");
    if (!hashes.add(segment.segment_sha256)) {
      fail(`backup plan contains duplicate segment ${segment.segment_sha256}`);
    }
    if (
      !Number.isSafeInteger(segment.first_observed_at_unix_ms) ||
      !Number.isSafeInteger(segment.last_observed_at_unix_ms) ||
      segment.first_observed_at_unix_ms > segment.last_observed_at_unix_ms
    ) {
      fail(`segment timestamps are invalid: ${segment.segment_file}`);
    }
    if (!/^20\d{2}-\d{2}-\d{2}$/.test(segment.utc_day)) {
      fail(`segment UTC day is invalid: ${segment.utc_day}`);
    }
  }
}

function validateLegacyPlan(plan: LegacyBackupPlan): void {
  if (plan.schema !== LEGACY_BACKUP_PLAN_SCHEMA) {
    fail(`unsupported legacy backup plan schema: ${plan.schema}`);
  }
  if (!Array.isArray(plan.artifacts)) {
    fail("legacy backup plan artifacts are invalid");
  }
  if (!Array.isArray(plan.blockers)) {
    fail("legacy backup plan blockers are invalid");
  }
  const blockerIds = new Set<string>();
  for (const blocker of plan.blockers) {
    assertSafeToken(blocker.code, "legacy blocker code");
    assertSafeToken(blocker.artifact_id, "legacy blocker artifact id");
    if (!blockerIds.add(blocker.artifact_id)) {
      fail(`legacy backup plan duplicates blocker ${blocker.artifact_id}`);
    }
    if (!blocker.local_path || !blocker.remediation) {
      fail(`legacy backup blocker is incomplete: ${blocker.artifact_id}`);
    }
  }
  const ids = new Set<string>();
  let totalBytes = 0;
  for (const artifact of plan.artifacts) {
    assertSafeToken(artifact.artifact_id, "legacy artifact id");
    assertSafeToken(artifact.kind, "legacy artifact kind");
    assertSafeToken(artifact.file_name, "legacy artifact file name");
    if (basename(artifact.local_path) !== artifact.file_name) {
      fail(`legacy artifact path/file mismatch: ${artifact.local_path}`);
    }
    assertSha256(artifact.sha256, "legacy artifact checksum");
    if (!ids.add(artifact.artifact_id)) {
      fail(`legacy backup plan contains duplicate ${artifact.artifact_id}`);
    }
    if (
      !Number.isSafeInteger(artifact.bytes) ||
      artifact.bytes < 0 ||
      !Number.isSafeInteger(artifact.modified_at_unix_ms) ||
      artifact.modified_at_unix_ms < 0
    ) {
      fail(`legacy artifact metadata is invalid: ${artifact.artifact_id}`);
    }
    totalBytes += artifact.bytes;
  }
  if (
    !Number.isSafeInteger(plan.total_artifact_bytes) ||
    plan.total_artifact_bytes !== totalBytes
  ) {
    fail("legacy backup plan total bytes do not match its artifacts");
  }
}

function sameSegmentSet(left: BackupPlan, right: BackupPlan): boolean {
  if (left.segments.length !== right.segments.length) return false;
  const rightHashes = new Set(
    right.segments.map((segment) => segment.segment_sha256),
  );
  return left.segments.every((segment) =>
    rightHashes.has(segment.segment_sha256),
  );
}

function sameLegacySet(
  left: LegacyBackupPlan,
  right: LegacyBackupPlan,
): boolean {
  if (left.artifacts.length !== right.artifacts.length) return false;
  if (left.blockers.length !== right.blockers.length) return false;
  const rightById = new Map(
    right.artifacts.map((artifact) => [artifact.artifact_id, artifact]),
  );
  const sameArtifacts = left.artifacts.every((artifact) => {
    const other = rightById.get(artifact.artifact_id);
    return (
      other !== undefined &&
      other.kind === artifact.kind &&
      other.file_name === artifact.file_name &&
      other.local_path === artifact.local_path &&
      other.sha256 === artifact.sha256 &&
      other.bytes === artifact.bytes &&
      other.modified_at_unix_ms === artifact.modified_at_unix_ms
    );
  });
  const rightBlockers = new Map(
    right.blockers.map((blocker) => [blocker.artifact_id, blocker]),
  );
  return (
    sameArtifacts &&
    left.blockers.every((blocker) => {
      const other = rightBlockers.get(blocker.artifact_id);
      return (
        other !== undefined &&
        other.code === blocker.code &&
        other.local_path === blocker.local_path &&
        other.remediation === blocker.remediation
      );
    })
  );
}

async function loadPlan(sb: string): Promise<BackupPlan> {
  const result = await run([sb, "body", "backup-plan", "--json"]);
  let plan: BackupPlan;
  try {
    plan = JSON.parse(result.stdout) as BackupPlan;
  } catch (error) {
    fail(`Switchback backup plan is not JSON: ${String(error)}`);
  }
  validatePlan(plan);
  return plan;
}

async function loadLegacyPlan(sb: string): Promise<LegacyBackupPlan> {
  const result = await run([sb, "body", "legacy-backup-plan", "--json"]);
  let plan: LegacyBackupPlan;
  try {
    plan = JSON.parse(result.stdout) as LegacyBackupPlan;
  } catch (error) {
    fail(`Switchback legacy backup plan is not JSON: ${String(error)}`);
  }
  plan.blockers ??= [];
  validateLegacyPlan(plan);
  return plan;
}

async function transferSegment(
  remote: string,
  remoteRoot: string,
  generation: number,
  segment: BackupPlanItem,
): Promise<ReceiptItem> {
  const dayPath = segment.utc_day.replaceAll("-", "/");
  const generationName = generation.toString().padStart(20, "0");
  const stagingRel = `.incoming/generation-${generationName}/${segment.segment_sha256}`;
  const finalRel = `segments/${dayPath}/${segment.segment_sha256}`;
  const manifestFile = basename(segment.manifest_path);
  const manifestSha = await localSha256(segment.manifest_path);
  if (manifestSha !== segment.manifest_sha256) {
    fail(`manifest changed after backup planning: ${segment.manifest_path}`);
  }

  await remoteAction(remote, "prepare", [remoteRoot, stagingRel]);
  const rsyncSsh =
    "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15";
  const remoteStaging = `${remote}:${remoteRoot}/${stagingRel}/`;
  for (const localPath of [segment.segment_path, segment.manifest_path]) {
    await run([
      "rsync",
      // Set no modes at all. `--chmod=F600` was rejected outright by the
      // openrsync macOS now ships as `rsync`, and `-p` then failed on the
      // receiver: the capture pool is aclmode=restricted, where chmod is denied
      // even to the owner, so any mode-setting flag fails with EPERM. The
      // dataset ACL grants permissions here; promote asserts it kept them
      // owner-only rather than trying to change them.
      "-rt",
      "--partial",
      "-e",
      rsyncSsh,
      localPath,
      remoteStaging,
    ]);
  }
  await remoteAction(remote, "promote", [
    remoteRoot,
    stagingRel,
    finalRel,
    segment.segment_file,
    segment.segment_sha256,
    manifestFile,
    manifestSha,
  ]);
  const remotePath = `${finalRel}/${segment.segment_file}`;
  const remoteManifestPath = `${finalRel}/${manifestFile}`;
  await remoteAction(remote, "verify", [
    remoteRoot,
    remotePath,
    segment.segment_sha256,
    remoteManifestPath,
    manifestSha,
  ]);

  return {
    segment_sha256: segment.segment_sha256,
    manifest_sha256: segment.manifest_sha256,
    remote_path: remotePath,
    remote_manifest_path: remoteManifestPath,
    remote_checksum_verified: true,
  };
}

async function transferLegacyArtifact(
  remote: string,
  remoteRoot: string,
  artifact: LegacyBackupPlanItem,
): Promise<LegacyReceiptItem> {
  const directory = artifact.kind === "directory";
  if (!directory) {
    const localSha = await localSha256(artifact.local_path);
    if (localSha !== artifact.sha256) {
      fail(`legacy artifact changed after planning: ${artifact.local_path}`);
    }
  }
  const stagingRel = `.incoming/legacy-${artifact.artifact_id}-${artifact.sha256}`;
  const finalRel = `legacy/${artifact.artifact_id}/${artifact.sha256}`;
  await remoteAction(remote, "prepare-legacy", [remoteRoot, stagingRel]);
  const rsyncSsh =
    "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15";
  await run([
    "rsync",
    // See the segment transfer above: no mode-setting flags survive openrsync
    // locally or aclmode=restricted remotely; promote-legacy asserts instead.
    "-rt",
    "--partial",
    "-e",
    rsyncSsh,
    artifact.local_path,
    `${remote}:${remoteRoot}/${stagingRel}/`,
  ]);
  await remoteAction(
    remote,
    directory ? "promote-legacy-tree" : "promote-legacy",
    [
      remoteRoot,
      stagingRel,
      finalRel,
      artifact.file_name,
      artifact.sha256,
    ],
  );
  const remotePath = `${finalRel}/${artifact.file_name}`;
  assertLegacyRemoteRelative(remotePath, "legacy receipt remote path");
  await remoteAction(
    remote,
    directory ? "verify-legacy-tree" : "verify-legacy",
    [remoteRoot, remotePath, artifact.sha256],
  );
  return {
    artifact_id: artifact.artifact_id,
    kind: artifact.kind,
    local_path: artifact.local_path,
    sha256: artifact.sha256,
    bytes: artifact.bytes,
    modified_at_unix_ms: artifact.modified_at_unix_ms,
    remote_path: remotePath,
    remote_checksum_verified: true,
  };
}

async function backupLegacyArtifacts(
  sb: string,
  remote: string,
  remoteRoot: string,
  initialPlan: LegacyBackupPlan,
): Promise<{
  accepted: boolean;
  transferred_artifacts: number;
  transferred_bytes: number;
}> {
  if (initialPlan.artifacts.length === 0) {
    return {
      accepted: false,
      transferred_artifacts: 0,
      transferred_bytes: 0,
    };
  }
  const receiptItems: LegacyReceiptItem[] = [];
  for (const artifact of initialPlan.artifacts) {
    receiptItems.push(
      await transferLegacyArtifact(remote, remoteRoot, artifact),
    );
  }
  const refreshed = await loadLegacyPlan(sb);
  if (!sameLegacySet(initialPlan, refreshed)) {
    fail("legacy artifact set changed during transfer");
  }
  const receipt = {
    schema: LEGACY_BACKUP_RECEIPT_SCHEMA,
    completed_at_unix_ms: Date.now(),
    remote_root: `${remote}:${remoteRoot}`,
    artifacts: receiptItems,
  };
  const receiptDir = await mkdtemp(
    join(tmpdir(), "switchback-capture-legacy-backup-receipt-"),
  );
  await chmod(receiptDir, 0o700);
  const receiptPath = join(receiptDir, "receipt.json");
  try {
    await Bun.write(receiptPath, `${JSON.stringify(receipt, null, 2)}\n`);
    await chmod(receiptPath, 0o600);
    const accepted = await run([
      sb,
      "body",
      "accept-legacy-backup-receipt",
      receiptPath,
      "--json",
    ]);
    const acceptance = JSON.parse(accepted.stdout) as {
      accepted?: boolean;
      artifacts?: number;
    };
    if (
      acceptance.accepted !== true ||
      acceptance.artifacts !== receiptItems.length
    ) {
      fail("Switchback did not confirm legacy backup receipt acceptance");
    }
  } finally {
    await rm(receiptDir, { recursive: true, force: true });
  }
  return {
    accepted: true,
    transferred_artifacts: receiptItems.length,
    transferred_bytes: initialPlan.total_artifact_bytes,
  };
}

async function reclaimVerifiedSegments(
  sb: string,
  remote: string,
  remoteRoot: string,
  keepDays: number,
): Promise<unknown> {
  const loaded = await run([
    sb,
    "body",
    "reclaim-plan",
    "--keep-days",
    String(keepDays),
    "--json",
  ]);
  const plan = JSON.parse(loaded.stdout) as ReclaimPlan;
  if (
    plan.schema !== RECLAIM_PLAN_SCHEMA ||
    plan.keep_days !== keepDays ||
    !Array.isArray(plan.segments)
  ) {
    fail("Switchback reclaim plan is invalid");
  }
  const expectedRemoteRoot = `${remote}:${remoteRoot}`;
  for (const segment of plan.segments) {
    assertSha256(segment.segment_sha256, "reclaim segment checksum");
    assertSha256(segment.manifest_sha256, "reclaim manifest checksum");
    assertRemoteRelative(segment.remote_path, "reclaim remote segment path");
    assertRemoteRelative(segment.remote_manifest_path, "reclaim remote manifest path");
    if (segment.remote_root !== expectedRemoteRoot) {
      fail(
        `reclaim catalog remote ${segment.remote_root} does not match ${expectedRemoteRoot}`,
      );
    }
    await remoteAction(remote, "verify", [
      remoteRoot,
      segment.remote_path,
      segment.segment_sha256,
      segment.remote_manifest_path,
      segment.manifest_sha256,
    ]);
  }
  const proof = {
    schema: RECLAIM_PROOF_SCHEMA,
    verified_at_unix_ms: Date.now(),
    segments: plan.segments.map((segment) => ({
      segment_sha256: segment.segment_sha256,
      manifest_sha256: segment.manifest_sha256,
      remote_path: segment.remote_path,
      remote_manifest_path: segment.remote_manifest_path,
      remote_checksums_verified: true,
    })),
  };
  const proofDir = await mkdtemp(
    join(tmpdir(), "switchback-capture-reclaim-proof-"),
  );
  await chmod(proofDir, 0o700);
  const proofPath = join(proofDir, "proof.json");
  try {
    await Bun.write(proofPath, `${JSON.stringify(proof, null, 2)}\n`);
    await chmod(proofPath, 0o600);
    const reclaimed = await run([
      sb,
      "body",
      "reclaim",
      proofPath,
      "--keep-days",
      String(keepDays),
      "--confirm",
      "--json",
    ]);
    return JSON.parse(reclaimed.stdout) as unknown;
  } finally {
    await rm(proofDir, { recursive: true, force: true });
  }
}

async function main(): Promise<void> {
  const mode = parseMode(process.argv.slice(2));
  const sb = process.env.SWITCHBACK_CLI || "sb";
  const remote = process.env.SWITCHBACK_BACKUP_REMOTE;
  const remoteRoot = process.env.SWITCHBACK_BACKUP_REMOTE_ROOT;
  if (!remote || !remoteRoot) {
    fail(
      "SWITCHBACK_BACKUP_REMOTE and SWITCHBACK_BACKUP_REMOTE_ROOT are required",
    );
  }
  assertSafeToken(remote, "backup remote");
  assertRemoteRoot(remoteRoot);
  const reclaim = envFlag("SWITCHBACK_BACKUP_RECLAIM");
  const keepDays = reclaimKeepDays();

  const plan = await loadPlan(sb);
  const legacyPlan = await loadLegacyPlan(sb);
  if (mode === "plan") {
    console.log(
      JSON.stringify(
        {
          schema: TRANSFER_RESULT_SCHEMA,
          mode,
          remote: `${remote}:${remoteRoot}`,
          plan,
          legacy_plan: legacyPlan,
        },
        null,
        2,
      ),
    );
    return;
  }

  if (legacyPlan.blockers.length > 0) {
    console.error(
      `legacy backup remains incomplete: ${legacyPlan.blockers
        .map((blocker) => `${blocker.artifact_id}:${blocker.code}`)
        .join(", ")}`,
    );
  }

  await remoteAction(remote, "ensure", [remoteRoot]);
  if (mode === "verify") {
    console.log(
      JSON.stringify(
        {
          schema: TRANSFER_RESULT_SCHEMA,
          mode,
          remote: `${remote}:${remoteRoot}`,
          generation: plan.next_generation,
          pending_segments: plan.segments.length,
          pending_bytes: plan.total_segment_bytes,
          pending_legacy_artifacts: legacyPlan.artifacts.length,
          pending_legacy_bytes: legacyPlan.total_artifact_bytes,
          legacy_complete: legacyPlan.blockers.length === 0,
          legacy_blockers: legacyPlan.blockers,
          writable: true,
        },
        null,
        2,
      ),
    );
    return;
  }

  const legacyReport = await backupLegacyArtifacts(
    sb,
    remote,
    remoteRoot,
    legacyPlan,
  );
  if (plan.segments.length === 0) {
    const reclaim_report = reclaim
      ? await reclaimVerifiedSegments(sb, remote, remoteRoot, keepDays)
      : null;
    console.log(
      JSON.stringify(
        {
          schema: TRANSFER_RESULT_SCHEMA,
          mode,
          remote: `${remote}:${remoteRoot}`,
          accepted: legacyReport.accepted,
          no_op: !legacyReport.accepted,
          generation: plan.next_generation,
          transferred_segments: 0,
          transferred_bytes: 0,
          legacy_receipt_accepted: legacyReport.accepted,
          transferred_legacy_artifacts: legacyReport.transferred_artifacts,
          transferred_legacy_bytes: legacyReport.transferred_bytes,
          legacy_complete: legacyPlan.blockers.length === 0,
          legacy_blockers: legacyPlan.blockers,
          reclaim_report,
        },
        null,
        2,
      ),
    );
    return;
  }

  const receiptItemsByHash = new Map<string, ReceiptItem>();
  let stablePlan = plan;
  let stabilized = false;
  for (let pass = 0; pass < 8; pass += 1) {
    for (const segment of stablePlan.segments) {
      if (!receiptItemsByHash.has(segment.segment_sha256)) {
        receiptItemsByHash.set(
          segment.segment_sha256,
          await transferSegment(
            remote,
            remoteRoot,
            stablePlan.next_generation,
            segment,
          ),
        );
      }
    }
    const refreshed = await loadPlan(sb);
    if (refreshed.next_generation !== stablePlan.next_generation) {
      fail(
        `backup generation changed during transfer: ${stablePlan.next_generation} -> ${refreshed.next_generation}`,
      );
    }
    if (sameSegmentSet(stablePlan, refreshed)) {
      stablePlan = refreshed;
      stabilized = true;
      break;
    }
    stablePlan = refreshed;
  }
  if (!stabilized) {
    fail("pending sealed segment set did not stabilize after 8 transfer passes");
  }
  const receiptItems = stablePlan.segments.map((segment) => {
    const item = receiptItemsByHash.get(segment.segment_sha256);
    if (!item) {
      fail(`stabilized segment lacks remote checksum proof: ${segment.segment_sha256}`);
    }
    return item;
  });

  const receipt = {
    schema: BACKUP_RECEIPT_SCHEMA,
    generation: stablePlan.next_generation,
    completed_at_unix_ms: Date.now(),
    verified_through_day:
      stablePlan.segments.length === 0
        ? null
        : stablePlan.segments[stablePlan.segments.length - 1]!.utc_day,
    remote_root: `${remote}:${remoteRoot}`,
    segments: receiptItems,
  };
  const receiptDir = await mkdtemp(
    join(tmpdir(), "switchback-capture-backup-receipt-"),
  );
  await chmod(receiptDir, 0o700);
  const receiptPath = join(receiptDir, "receipt.json");
  try {
    await Bun.write(receiptPath, `${JSON.stringify(receipt, null, 2)}\n`);
    await chmod(receiptPath, 0o600);
    const accepted = await run([
      sb,
      "body",
      "accept-backup-receipt",
      receiptPath,
      "--json",
    ]);
    const acceptance = JSON.parse(accepted.stdout) as {
      accepted?: boolean;
      generation?: number;
    };
    if (
      acceptance.accepted !== true ||
      acceptance.generation !== stablePlan.next_generation
    ) {
      fail("Switchback did not confirm backup receipt acceptance");
    }
  } finally {
    await rm(receiptDir, { recursive: true, force: true });
  }

  const reclaim_report = reclaim
    ? await reclaimVerifiedSegments(sb, remote, remoteRoot, keepDays)
    : null;
  console.log(
    JSON.stringify(
      {
        schema: TRANSFER_RESULT_SCHEMA,
        mode,
        remote: `${remote}:${remoteRoot}`,
        accepted: true,
        generation: stablePlan.next_generation,
        transferred_segments: receiptItems.length,
        transferred_bytes: stablePlan.total_segment_bytes,
        legacy_receipt_accepted: legacyReport.accepted,
        transferred_legacy_artifacts: legacyReport.transferred_artifacts,
        transferred_legacy_bytes: legacyReport.transferred_bytes,
        legacy_complete: legacyPlan.blockers.length === 0,
        legacy_blockers: legacyPlan.blockers,
        verified_through_day: receipt.verified_through_day,
        reclaim_report,
      },
      null,
      2,
    ),
  );
}

await main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
