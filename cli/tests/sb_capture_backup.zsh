#!/bin/zsh
set -euo pipefail
zmodload -F zsh/stat b:zstat

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

file_mtime_seconds() {
  local target="${1:?missing target}"
  zstat +mtime "$target"
}

CLI_ROOT="${0:A:h:h}"
TOOL="${CLI_ROOT}/switchback-capture-backup.ts"
export FAKE_REMOTE_FS="${TMPDIR}/remote"
export ACCEPTED_RECEIPT="${TMPDIR}/accepted-receipt.json"
export ACCEPTED_LEGACY_RECEIPT="${TMPDIR}/accepted-legacy-receipt.json"
export RECLAIM_PROOF="${TMPDIR}/reclaim-proof.json"
export CALL_LOG="${TMPDIR}/calls.log"
segment="${TMPDIR}/capture-1.sbcap"
manifest="${segment}.manifest.json"
print -n -- "sealed-segment-payload" > "$segment"
segment_sha="$(shasum -a 256 "$segment" | awk '{print $1}')"
print -r -- '{"schema_version":"switchback/capture-segment@1","sealed":true}' > "$manifest"
manifest_sha="$(shasum -a 256 "$manifest" | awk '{print $1}')"
second_segment="${TMPDIR}/capture-2.sbcap"
second_manifest="${second_segment}.manifest.json"
print -n -- "newly-sealed-payload" > "$second_segment"
second_segment_sha="$(shasum -a 256 "$second_segment" | awk '{print $1}')"
print -r -- '{"schema_version":"switchback/capture-segment@1","sealed":true}' > "$second_manifest"
export TEST_SEGMENT="$segment"
export TEST_MANIFEST="$manifest"
export TEST_SEGMENT_SHA="$segment_sha"
export TEST_MANIFEST_SHA="$manifest_sha"
export TEST_SECOND_SEGMENT="$second_segment"
export TEST_SECOND_MANIFEST="$second_manifest"
export TEST_SECOND_SEGMENT_SHA="$second_segment_sha"
export TEST_SECOND_MANIFEST_SHA="$(shasum -a 256 "$second_manifest" | awk '{print $1}')"
export PLAN_CALL_COUNT="${TMPDIR}/plan-call-count"
legacy_jsonl="${TMPDIR}/tap-bodies.jsonl"
print -n -- "frozen-legacy-capture" > "$legacy_jsonl"
legacy_jsonl_sha="$(shasum -a 256 "$legacy_jsonl" | awk '{print $1}')"
legacy_jsonl_mtime_ms="$(( $(file_mtime_seconds "$legacy_jsonl") * 1000 ))"
export TEST_LEGACY_JSONL="$legacy_jsonl"
export TEST_LEGACY_JSONL_SHA="$legacy_jsonl_sha"
export TEST_LEGACY_JSONL_MTIME_MS="$legacy_jsonl_mtime_ms"
legacy_blob_dir="${TMPDIR}/legacy-blobs"
mkdir -p "${legacy_blob_dir}/sha256/aa"
print -n -- "legacy-blob-one" > "${legacy_blob_dir}/sha256/aa/aa-one.zst"
print -n -- "legacy-blob-two" > "${legacy_blob_dir}/sha256/aa/aa-two.zst"
legacy_blob_sha="$(
  cd "$legacy_blob_dir"
  find . -type f -print0 | LC_ALL=C sort -z |
    while IFS= read -r -d '' relative; do
      relative="${relative#./}"
      sha="$(shasum -a 256 "$relative" | awk '{print $1}')"
      bytes="$(wc -c < "$relative" | tr -d '[:space:]')"
      printf '%s\t%s\t%s\n' "$sha" "$bytes" "$relative"
    done |
    shasum -a 256 | awk '{print $1}'
)"
legacy_blob_bytes=30
legacy_blob_mtime_ms=1000
export TEST_LEGACY_BLOB_DIR="$legacy_blob_dir"
export TEST_LEGACY_BLOB_SHA="$legacy_blob_sha"
export TEST_LEGACY_BLOB_BYTES="$legacy_blob_bytes"
export TEST_LEGACY_BLOB_MTIME_MS="$legacy_blob_mtime_ms"

mkdir -p "${TMPDIR}/bin" "$FAKE_REMOTE_FS"

cat > "${TMPDIR}/bin/sb" <<'FAKE'
#!/bin/zsh
set -euo pipefail
print -r -- "sb:$*" >> "$CALL_LOG"
if [[ "$1" == body && "$2" == backup-plan && "$3" == --json ]]; then
  count=0
  [[ ! -f "$PLAN_CALL_COUNT" ]] || count="$(<"$PLAN_CALL_COUNT")"
  count=$((count + 1))
  print -r -- "$count" > "$PLAN_CALL_COUNT"
  if [[ "${FAKE_PLAN_GROWS:-0}" == 1 && "$count" -ge 2 ]]; then
    print -r -- "{
      \"schema\":\"switchback/capture-backup-plan@1\",
      \"next_generation\":7,
      \"segments\":[{
        \"segment_file\":\"capture-1.sbcap\",
        \"segment_path\":\"${TEST_SEGMENT}\",
        \"manifest_path\":\"${TEST_MANIFEST}\",
        \"segment_sha256\":\"${TEST_SEGMENT_SHA}\",
        \"manifest_sha256\":\"${TEST_MANIFEST_SHA}\",
        \"segment_bytes\":22,
        \"record_count\":1,
        \"first_observed_at_unix_ms\":1753401600000,
        \"last_observed_at_unix_ms\":1753401601000,
        \"utc_day\":\"2026-07-25\"
      },{
        \"segment_file\":\"capture-2.sbcap\",
        \"segment_path\":\"${TEST_SECOND_SEGMENT}\",
        \"manifest_path\":\"${TEST_SECOND_MANIFEST}\",
        \"segment_sha256\":\"${TEST_SECOND_SEGMENT_SHA}\",
        \"manifest_sha256\":\"${TEST_SECOND_MANIFEST_SHA}\",
        \"segment_bytes\":20,
        \"record_count\":1,
        \"first_observed_at_unix_ms\":1753401602000,
        \"last_observed_at_unix_ms\":1753401603000,
        \"utc_day\":\"2026-07-25\"
      }],
      \"total_segment_bytes\":42
    }"
  else
    print -r -- "{
    \"schema\":\"switchback/capture-backup-plan@1\",
    \"next_generation\":7,
    \"segments\":[{
      \"segment_file\":\"capture-1.sbcap\",
      \"segment_path\":\"${TEST_SEGMENT}\",
      \"manifest_path\":\"${TEST_MANIFEST}\",
      \"segment_sha256\":\"${TEST_SEGMENT_SHA}\",
      \"manifest_sha256\":\"${TEST_MANIFEST_SHA}\",
      \"segment_bytes\":22,
      \"record_count\":1,
      \"first_observed_at_unix_ms\":1753401600000,
      \"last_observed_at_unix_ms\":1753401601000,
      \"utc_day\":\"2026-07-25\"
    }],
    \"total_segment_bytes\":22
    }"
  fi
  exit 0
fi
if [[ "$1" == body && "$2" == legacy-backup-plan && "$3" == --json ]]; then
  if [[ "${FAKE_LEGACY_BLOCKED:-0}" == 1 ]]; then
    print -r -- '{
      "schema":"switchback/capture-legacy-backup-plan@1",
      "artifacts":[],
      "total_artifact_bytes":0,
      "blockers":[{
        "code":"v2_index_missing_or_legacy_index_active",
        "artifact_id":"legacy-segment-index",
        "local_path":"/tmp/index.sqlite",
        "remediation":"restart capture writers onto v2"
      }]
    }'
  elif [[ "${FAKE_LEGACY_TREE:-0}" == 1 ]]; then
    print -r -- "{
      \"schema\":\"switchback/capture-legacy-backup-plan@1\",
      \"artifacts\":[{
        \"artifact_id\":\"legacy-blob-archive\",
        \"kind\":\"directory\",
        \"file_name\":\"legacy-blobs\",
        \"local_path\":\"${TEST_LEGACY_BLOB_DIR}\",
        \"sha256\":\"${TEST_LEGACY_BLOB_SHA}\",
        \"bytes\":${TEST_LEGACY_BLOB_BYTES},
        \"modified_at_unix_ms\":${TEST_LEGACY_BLOB_MTIME_MS}
      }],
      \"total_artifact_bytes\":${TEST_LEGACY_BLOB_BYTES},
      \"blockers\":[]
    }"
  elif [[ -f "$ACCEPTED_LEGACY_RECEIPT" ]]; then
    print -r -- '{"schema":"switchback/capture-legacy-backup-plan@1","artifacts":[],"total_artifact_bytes":0}'
  else
    print -r -- "{
      \"schema\":\"switchback/capture-legacy-backup-plan@1\",
      \"artifacts\":[{
        \"artifact_id\":\"legacy-jsonl\",
        \"kind\":\"jsonl\",
        \"file_name\":\"tap-bodies.jsonl\",
        \"local_path\":\"${TEST_LEGACY_JSONL}\",
        \"sha256\":\"${TEST_LEGACY_JSONL_SHA}\",
        \"bytes\":21,
        \"modified_at_unix_ms\":${TEST_LEGACY_JSONL_MTIME_MS}
      }],
      \"total_artifact_bytes\":21
    }"
  fi
  exit 0
fi
if [[ "$1" == body && "$2" == accept-backup-receipt && "$4" == --json ]]; then
  cp "$3" "$ACCEPTED_RECEIPT"
  print -r -- '{"schema":"switchback/capture-backup-accept@1","accepted":true,"generation":7}'
  exit 0
fi
if [[ "$1" == body && "$2" == accept-legacy-backup-receipt && "$4" == --json ]]; then
  cp "$3" "$ACCEPTED_LEGACY_RECEIPT"
  print -r -- '{"schema":"switchback/capture-legacy-backup-accept@1","accepted":true,"artifacts":1}'
  exit 0
fi
if [[ "$1" == body && "$2" == reclaim-plan ]]; then
  print -r -- "{
    \"schema\":\"switchback/capture-reclaim-plan@1\",
    \"keep_days\":14,
    \"cutoff_unix_ms\":1750000000000,
    \"segments\":[{
      \"segment_file\":\"capture-1.sbcap\",
      \"segment_path\":\"${TEST_SEGMENT}\",
      \"manifest_path\":\"${TEST_MANIFEST}\",
      \"segment_sha256\":\"${TEST_SEGMENT_SHA}\",
      \"manifest_sha256\":\"${TEST_MANIFEST_SHA}\",
      \"segment_bytes\":22,
      \"utc_day\":\"2026-07-25\",
      \"remote_root\":\"truenas:/mnt/tank/personal/backups/machines/switchback-capture-v2\",
      \"remote_path\":\"segments/2026/07/25/${TEST_SEGMENT_SHA}/capture-1.sbcap\",
      \"remote_manifest_path\":\"segments/2026/07/25/${TEST_SEGMENT_SHA}/capture-1.sbcap.manifest.json\"
    }],
    \"total_segment_bytes\":22
  }"
  exit 0
fi
if [[ "$1" == body && "$2" == reclaim && "$7" == --json ]]; then
  cp "$3" "$RECLAIM_PROOF"
  print -r -- '{"schema":"switchback/capture-reclaim-report@1","dry_run":false,"keep_days":14,"candidate_segments":1,"candidate_bytes":22,"reclaimed_segments":1,"reclaimed_bytes":22}'
  exit 0
fi
exit 9
FAKE
chmod +x "${TMPDIR}/bin/sb"

cat > "${TMPDIR}/bin/rsync" <<'FAKE'
#!/bin/zsh
set -euo pipefail
print -r -- "rsync:$*" >> "$CALL_LOG"
source_path="${@[-2]}"
remote_destination="${@[-1]}"
remote_path="${remote_destination#*:}"
destination="${FAKE_REMOTE_FS}${remote_path}"
mkdir -p "$destination"
if [[ -d "$source_path" ]]; then
  cp -R "$source_path" "$destination/"
else
  cp "$source_path" "$destination/"
fi
FAKE
chmod +x "${TMPDIR}/bin/rsync"

cat > "${TMPDIR}/bin/ssh" <<'FAKE'
#!/bin/zsh
set -euo pipefail
print -r -- "ssh:$*" >> "$CALL_LOG"
while (( $# )); do
  if [[ "$1" == "truenas" ]]; then
    shift
    break
  fi
  shift
done
[[ "$1" == bash && "$2" == -s && "$3" == -- ]] || exit 8
shift 3
action="$1"
shift
tree_fingerprint() {
  local tree="$1"
  (
    cd "$tree"
    find . -type f -print0 | LC_ALL=C sort -z |
      while IFS= read -r -d '' relative; do
        relative="${relative#./}"
        sha="$(shasum -a 256 "$relative" | awk '{print $1}')"
        bytes="$(wc -c < "$relative" | tr -d '[:space:]')"
        printf '%s\t%s\t%s\n' "$sha" "$bytes" "$relative"
      done
  ) | shasum -a 256 | awk '{print $1}'
}
case "$action" in
  ensure)
    root="$1"
    mkdir -p "${FAKE_REMOTE_FS}${root}/segments" "${FAKE_REMOTE_FS}${root}/.incoming"
    ;;
  prepare)
    root="$1"
    staging_rel="$2"
    mkdir -p "${FAKE_REMOTE_FS}${root}/${staging_rel}"
    ;;
  promote)
    [[ "${FAKE_PROMOTE_FAIL:-0}" == 0 ]] || exit 12
    root="$1"
    staging_rel="$2"
    final_rel="$3"
    segment_file="$4"
    expected_segment_sha="$5"
    manifest_file="$6"
    expected_manifest_sha="$7"
    staging="${FAKE_REMOTE_FS}${root}/${staging_rel}"
    final="${FAKE_REMOTE_FS}${root}/${final_rel}"
    [[ "$(shasum -a 256 "${staging}/${segment_file}" | awk '{print $1}')" == "$expected_segment_sha" ]]
    [[ "$(shasum -a 256 "${staging}/${manifest_file}" | awk '{print $1}')" == "$expected_manifest_sha" ]]
    mkdir -p "${final:h}"
    if [[ -d "$final" ]]; then
      rm -rf "$staging"
    else
      mv "$staging" "$final"
    fi
    ;;
  verify)
    root="$1"
    segment_rel="$2"
    expected_segment_sha="$3"
    manifest_rel="$4"
    expected_manifest_sha="$5"
    [[ "$(shasum -a 256 "${FAKE_REMOTE_FS}${root}/${segment_rel}" | awk '{print $1}')" == "$expected_segment_sha" ]]
    [[ "$(shasum -a 256 "${FAKE_REMOTE_FS}${root}/${manifest_rel}" | awk '{print $1}')" == "$expected_manifest_sha" ]]
    ;;
  prepare-legacy)
    root="$1"
    staging_rel="$2"
    mkdir -p "${FAKE_REMOTE_FS}${root}/${staging_rel}"
    ;;
  promote-legacy)
    root="$1"
    staging_rel="$2"
    final_rel="$3"
    file_name="$4"
    expected_sha="$5"
    staging="${FAKE_REMOTE_FS}${root}/${staging_rel}"
    final="${FAKE_REMOTE_FS}${root}/${final_rel}"
    [[ "$(shasum -a 256 "${staging}/${file_name}" | awk '{print $1}')" == "$expected_sha" ]]
    mkdir -p "${final:h}"
    if [[ -d "$final" ]]; then
      rm -rf "$staging"
    else
      mv "$staging" "$final"
    fi
    ;;
  verify-legacy)
    root="$1"
    artifact_rel="$2"
    expected_sha="$3"
    [[ "$(shasum -a 256 "${FAKE_REMOTE_FS}${root}/${artifact_rel}" | awk '{print $1}')" == "$expected_sha" ]]
    ;;
  promote-legacy-tree)
    root="$1"
    staging_rel="$2"
    final_rel="$3"
    directory_name="$4"
    expected_sha="$5"
    staging="${FAKE_REMOTE_FS}${root}/${staging_rel}"
    final="${FAKE_REMOTE_FS}${root}/${final_rel}"
    [[ "$(tree_fingerprint "${staging}/${directory_name}")" == "$expected_sha" ]]
    mkdir -p "${final:h}"
    if [[ -d "$final" ]]; then
      rm -rf "$staging"
    else
      mv "$staging" "$final"
    fi
    ;;
  verify-legacy-tree)
    root="$1"
    artifact_rel="$2"
    expected_sha="$3"
    [[ "$(tree_fingerprint "${FAKE_REMOTE_FS}${root}/${artifact_rel}")" == "$expected_sha" ]]
    ;;
  *)
    exit 7
    ;;
esac
FAKE
chmod +x "${TMPDIR}/bin/ssh"

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: ${needle}\nactual:\n${haystack}"
}

local_tree_fingerprint() {
  local tree="$1"
  (
    cd "$tree"
    find . -type f -print0 | LC_ALL=C sort -z |
      while IFS= read -r -d '' relative; do
        relative="${relative#./}"
        sha="$(shasum -a 256 "$relative" | awk '{print $1}')"
        bytes="$(wc -c < "$relative" | tr -d '[:space:]')"
        printf '%s\t%s\t%s\n' "$sha" "$bytes" "$relative"
      done
  ) | shasum -a 256 | awk '{print $1}'
}

run_output="$(
  PATH="${TMPDIR}/bin:${PATH}" \
  SWITCHBACK_CLI="${TMPDIR}/bin/sb" \
  SWITCHBACK_BACKUP_REMOTE="truenas" \
  SWITCHBACK_BACKUP_REMOTE_ROOT="/mnt/tank/personal/backups/machines/switchback-capture-v2" \
  bun "$TOOL"
)"
assert_contains "$run_output" '"accepted": true'
assert_contains "$run_output" '"generation": 7'

remote_dir="${FAKE_REMOTE_FS}/mnt/tank/personal/backups/machines/switchback-capture-v2/segments/2026/07/25/${segment_sha}"
[[ -f "${remote_dir}/capture-1.sbcap" ]] || fail "sealed segment was not promoted"
[[ -f "${remote_dir}/capture-1.sbcap.manifest.json" ]] || fail "segment manifest was not promoted"
[[ "$(shasum -a 256 "${remote_dir}/capture-1.sbcap" | awk '{print $1}')" == "$segment_sha" ]] \
  || fail "promoted segment checksum differs"
[[ -f "$ACCEPTED_RECEIPT" ]] || fail "Switchback receipt was not accepted"
legacy_remote_dir="${FAKE_REMOTE_FS}/mnt/tank/personal/backups/machines/switchback-capture-v2/legacy/legacy-jsonl/${legacy_jsonl_sha}"
[[ -f "${legacy_remote_dir}/tap-bodies.jsonl" ]] || fail "frozen legacy artifact was not promoted"
[[ -f "$ACCEPTED_LEGACY_RECEIPT" ]] || fail "Switchback legacy receipt was not accepted"
legacy_receipt="$(<"$ACCEPTED_LEGACY_RECEIPT")"
assert_contains "$legacy_receipt" '"schema": "switchback/capture-legacy-backup@1"'
assert_contains "$legacy_receipt" '"artifact_id": "legacy-jsonl"'
assert_contains "$legacy_receipt" '"remote_checksum_verified": true'
assert_contains "$legacy_receipt" '"remote_path": "legacy/legacy-jsonl/'

receipt="$(<"$ACCEPTED_RECEIPT")"
assert_contains "$receipt" '"schema": "switchback/capture-backup@2"'
assert_contains "$receipt" '"generation": 7'
assert_contains "$receipt" '"verified_through_day": "2026-07-25"'
assert_contains "$receipt" '"remote_checksum_verified": true'
assert_contains "$receipt" '"remote_path": "segments/2026/07/25/'
assert_contains "$receipt" "\"manifest_sha256\": \"${manifest_sha}\""
assert_contains "$receipt" '"remote_manifest_path": "segments/2026/07/25/'

blocked_plan="$(
  PATH="${TMPDIR}/bin:${PATH}" \
  SWITCHBACK_CLI="${TMPDIR}/bin/sb" \
  SWITCHBACK_BACKUP_REMOTE="truenas" \
  SWITCHBACK_BACKUP_REMOTE_ROOT="/mnt/tank/personal/backups/machines/switchback-capture-v2" \
  FAKE_LEGACY_BLOCKED=1 \
  bun "$TOOL" --plan
)"
assert_contains "$blocked_plan" '"legacy-segment-index"'
assert_contains "$blocked_plan" '"v2_index_missing_or_legacy_index_active"'
assert_contains "$blocked_plan" '"restart capture writers onto v2"'

rm -rf "$FAKE_REMOTE_FS"
rm -f "$ACCEPTED_RECEIPT" "$ACCEPTED_LEGACY_RECEIPT" "$PLAN_CALL_COUNT"
tree_output="$(
  PATH="${TMPDIR}/bin:${PATH}" \
  SWITCHBACK_CLI="${TMPDIR}/bin/sb" \
  SWITCHBACK_BACKUP_REMOTE="truenas" \
  SWITCHBACK_BACKUP_REMOTE_ROOT="/mnt/tank/personal/backups/machines/switchback-capture-v2" \
  FAKE_LEGACY_TREE=1 \
  bun "$TOOL"
)"
assert_contains "$tree_output" '"legacy_complete": true'
tree_remote_dir="${FAKE_REMOTE_FS}/mnt/tank/personal/backups/machines/switchback-capture-v2/legacy/legacy-blob-archive/${legacy_blob_sha}/legacy-blobs"
[[ -f "${tree_remote_dir}/sha256/aa/aa-one.zst" ]] ||
  fail "legacy blob tree was not promoted"
[[ "$(local_tree_fingerprint "$tree_remote_dir")" == "$legacy_blob_sha" ]] ||
  fail "promoted legacy blob tree fingerprint differs"
tree_receipt="$(<"$ACCEPTED_LEGACY_RECEIPT")"
assert_contains "$tree_receipt" '"artifact_id": "legacy-blob-archive"'
assert_contains "$tree_receipt" '"kind": "directory"'
assert_contains "$tree_receipt" '"remote_checksum_verified": true'

rm -f "$PLAN_CALL_COUNT" "$RECLAIM_PROOF"
reclaim_output="$(
  PATH="${TMPDIR}/bin:${PATH}" \
  SWITCHBACK_CLI="${TMPDIR}/bin/sb" \
  SWITCHBACK_BACKUP_REMOTE="truenas" \
  SWITCHBACK_BACKUP_REMOTE_ROOT="/mnt/tank/personal/backups/machines/switchback-capture-v2" \
  SWITCHBACK_BACKUP_RECLAIM=1 \
  SWITCHBACK_BACKUP_KEEP_DAYS=14 \
  bun "$TOOL"
)"
assert_contains "$reclaim_output" '"reclaimed_segments": 1'
[[ -f "$RECLAIM_PROOF" ]] || fail "remote re-verification proof was not passed to Switchback"
reclaim_proof="$(<"$RECLAIM_PROOF")"
assert_contains "$reclaim_proof" '"remote_checksums_verified": true'
assert_contains "$reclaim_proof" "\"segment_sha256\": \"${segment_sha}\""

rm -rf "$FAKE_REMOTE_FS"
rm -f "$ACCEPTED_RECEIPT" "$PLAN_CALL_COUNT"
growth_output="$(
  PATH="${TMPDIR}/bin:${PATH}" \
  SWITCHBACK_CLI="${TMPDIR}/bin/sb" \
  SWITCHBACK_BACKUP_REMOTE="truenas" \
  SWITCHBACK_BACKUP_REMOTE_ROOT="/mnt/tank/personal/backups/machines/switchback-capture-v2" \
  FAKE_PLAN_GROWS=1 \
  bun "$TOOL"
)"
assert_contains "$growth_output" '"transferred_segments": 2'
growth_receipt="$(<"$ACCEPTED_RECEIPT")"
assert_contains "$growth_receipt" "$second_segment_sha"
second_remote_dir="${FAKE_REMOTE_FS}/mnt/tank/personal/backups/machines/switchback-capture-v2/segments/2026/07/25/${second_segment_sha}"
[[ -f "${second_remote_dir}/capture-2.sbcap" ]] \
  || fail "segment sealed during transfer was omitted from the exact receipt"

rm -rf "$FAKE_REMOTE_FS"
rm -f "$ACCEPTED_RECEIPT" "$PLAN_CALL_COUNT"
if PATH="${TMPDIR}/bin:${PATH}" \
  SWITCHBACK_CLI="${TMPDIR}/bin/sb" \
  SWITCHBACK_BACKUP_REMOTE="truenas" \
  SWITCHBACK_BACKUP_REMOTE_ROOT="/mnt/tank/personal/backups/machines/switchback-capture-v2" \
  FAKE_PROMOTE_FAIL=1 \
  bun "$TOOL" >/dev/null 2>&1; then
  fail "remote checksum/promotion failure was accepted"
fi
[[ ! -f "$ACCEPTED_RECEIPT" ]] || fail "receipt accepted after remote verification failure"

rm -f "$PLAN_CALL_COUNT"
if PATH="${TMPDIR}/bin:${PATH}" \
  SWITCHBACK_CLI="${TMPDIR}/bin/sb" \
  SWITCHBACK_BACKUP_REMOTE="truenas" \
  SWITCHBACK_BACKUP_REMOTE_ROOT="/mnt/tank/../../tmp/switchback-capture" \
  bun "$TOOL" --plan >/dev/null 2>&1; then
  fail "remote backup root accepted path traversal"
fi

print "ok - sealed segment backup transfer and receipt acceptance"
